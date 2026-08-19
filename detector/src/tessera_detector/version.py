"""What determines this detector's output, as one digest.

The gateway caches spans and keys them by this value, so it has to change
whenever the same text would produce different spans. That is the pinned
weights *and* both catalogs: a threshold edit in `ner.yaml` changes what is
detected without touching `HF_REVISION`, and a cache that missed it would keep
serving the old thresholds.

`model_id` is a caller-supplied argument rather than a constant computed in
here on purpose: `HF_REVISION` names the pinned snapshot, not what is
necessarily loaded. `TESSERA_NER_MODEL` can override the weights at start-up,
and building `model_id` from the constant regardless would report the pinned
snapshot's identity for weights that are not it. `pipeline.build_detector`
is what decides the honest value — the pinned constant when no NER weights
are loaded, a digest of the actually-loaded weight bytes when they are — and
hands it down through `Detector.model_id`.
"""

import hashlib
from collections.abc import Iterable
from importlib import resources

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


def detector_version(model_id: str) -> str:
    """`model_id` has no default deliberately: a forgotten argument here is
    exactly the bug this signature exists to make impossible to write by
    accident — silently falling back to the pinned constant regardless of
    what is actually loaded."""
    catalog_dir = resources.files("tessera_detector") / "catalog"
    return version_from(
        model_id,
        [(catalog_dir / name).read_bytes() for name in CATALOGS],
    )


__all__ = ["CATALOGS", "detector_version", "version_from"]
