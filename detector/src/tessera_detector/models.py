"""Where the NER weights live. They are never committed — hundreds of megabytes.

Lookup order is TESSERA_NER_MODEL, then the user cache. A variable pointing at a
missing path is an error: a typo must not quietly downgrade the pipeline to the
deterministic layer alone.
"""

import hashlib
import os
from pathlib import Path

MODEL_NAME = "gliner_multi-v2.1"
# The upstream `urchade/gliner_multi-v2.1` repo publishes PyTorch weights only.
# This mirror re-exports the same model with ONNX graphs (`onnx/model.onnx` and
# quantized variants), which is what `GlinerRecognizer` loads via onnxruntime.
HF_REPO_ID = f"onnx-community/{MODEL_NAME}"
# Pinned: the published metrics are only reproducible if the same Tessera commit
# always runs the same weights. Bump deliberately, and re-measure when you do.
HF_REVISION = "6ddaeb9413b0e71ad8457da1aab378a165b24058"
# What GlinerRecognizer needs to load. snapshot_download creates the directory
# before it finishes, so directory existence alone would let an interrupted
# download masquerade as installed weights and crash the loader instead of
# falling back to the deterministic layer.
REQUIRED_ARTIFACTS = ("onnx/model.onnx", "config.json")


class ModelUnavailable(Exception):
    """The NER layer was required but no weights were found."""


def model_cache_dir() -> Path:
    # The revision is part of the directory name, not just of the download call:
    # a cache filled before a revision bump would otherwise keep serving stale
    # weights to every automatic scan until someone re-ran `make model`.
    return Path.home() / ".cache" / "tessera" / "models" / f"{MODEL_NAME}@{HF_REVISION[:12]}"


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


def weights_digest(path: Path) -> str:
    """A digest over the artifact bytes actually found at `path` — not the
    path itself, which `TESSERA_NER_MODEL` can point anywhere, and the same
    path can hold different bytes across a redeploy. This is what the
    version the gateway caches by has to name honestly: two detectors that
    both call themselves the pinned snapshot but load different weights
    (one overridden, one not) must not report the same version.

    Called once, when the model is resolved — not per request. `onnx/model.onnx`
    alone runs over a gigabyte; hashing it on every `/detect` call would add
    real latency to the one feature (the gateway's cache) whose entire point
    is to avoid work on the common path. Measured warm-cache: ~0.75 s for the
    full graph, paid once at startup alongside the model load itself, which
    already costs "seconds" (see the top-level README).

    Raises whatever the read raises if a required artifact cannot be read —
    deliberately not caught here. `find_model` has already confirmed these
    paths exist by the time this runs; a read failure past that point means
    the weights' identity cannot be established, and reporting a version
    anyway would silently reintroduce the bug this function exists to close.
    Fail the startup instead of guessing.
    """
    digest = hashlib.sha256()
    for name in REQUIRED_ARTIFACTS:
        digest.update(hashlib.sha256((path / name).read_bytes()).digest())
    return digest.hexdigest()


__all__ = [
    "HF_REPO_ID",
    "HF_REVISION",
    "MODEL_NAME",
    "REQUIRED_ARTIFACTS",
    "ModelUnavailable",
    "find_model",
    "model_cache_dir",
    "weights_digest",
]
