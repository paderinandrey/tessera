"""What determines this detector's output, as one digest.

The gateway caches spans and keys them by this value, so it has to change
whenever the same text would produce different spans. That is the pinned
weights *and* both catalogs: a threshold edit in `ner.yaml` changes what is
detected without touching `HF_REVISION`, and a cache that missed it would keep
serving the old thresholds.
"""

import hashlib
from collections.abc import Iterable
from importlib import resources

from .models import HF_REVISION, MODEL_NAME

CATALOGS = ("identifiers.yaml", "ner.yaml")


def version_from(model_id: str, catalogs: Iterable[bytes]) -> str:
    """The digest, over inputs the caller supplies. Pure, so it is testable
    without a populated model cache or a rewritten package resource."""
    digest = hashlib.sha256()
    digest.update(model_id.encode("utf-8"))
    for blob in catalogs:
        # Each catalog is hashed before it is folded in, so that a byte moving
        # from the end of one to the start of the next cannot leave the total
        # unchanged the way plain concatenation would.
        digest.update(hashlib.sha256(blob).digest())
    return digest.hexdigest()[:32]


def detector_version() -> str:
    catalog_dir = resources.files("tessera_detector") / "catalog"
    return version_from(
        f"{MODEL_NAME}@{HF_REVISION}",
        [(catalog_dir / name).read_bytes() for name in CATALOGS],
    )


__all__ = ["CATALOGS", "detector_version", "version_from"]
