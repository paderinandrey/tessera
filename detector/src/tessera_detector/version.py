"""What determines this detector's output, as one digest.

The gateway caches spans and keys them by this value, so it has to change
whenever the same text would produce different spans. That is the pinned
weights, both catalogs, *and* this package's own source: a threshold edit in
`ner.yaml` changes what is detected without touching `HF_REVISION`, and a
change to chunking or conflict resolution changes it without touching the
weights or the catalogs either — a cache that missed either would keep
serving stale output. See `source_digest` for the boundary of what the
source half of this actually covers.

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
from pathlib import Path

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


def source_digest(root: Path | None = None) -> str:
    """A digest over this package's own `.py` sources, in sorted path order.

    `root` defaults to this package's own installed directory and exists as
    a parameter only so a test can point it at a temporary tree instead —
    the same reason `weights_digest` takes `path` rather than resolving it
    internally. No production call site passes it.

    `model_id` names the weights; the catalogs name the detection rules;
    neither names the code that turns one into the other. Chunking, the
    token window, conflict resolution — a change to any of those changes
    spans with the weights and catalogs untouched, and a gateway caching
    across that upgrade would keep serving offsets a new build no longer
    produces. Real, not hypothetical: the container image carries this
    code, `make model` fetches weights separately, so a new image on old
    weights is exactly this case.

    Every `.py` file under this package, deliberately not narrowed to
    "the ones that compute spans" — `api.py` and `cli.py` do not, but
    guessing wrong about which files matter would under-cover the way
    `REQUIRED_ARTIFACTS` under-covered the weights (see `models.py`), and
    over-invalidating a cache entry costs a rescan, not a wrong answer.

    Boundary, stated rather than left implicit: this sees changes to
    tessera_detector's own source and nothing else. A dependency upgrade —
    GLiNER, onnxruntime, the tokenizer library — can change spans too
    (a new tokenizer release changing token boundaries, for one), and this
    digest does not see it.

    Whether `uv.lock` belongs in this digest was considered and set aside
    rather than added by reflex. The lockfile pins every dependency in the
    project, including pytest, ruff and mypy, none of which run at
    inference time; hashing it would invalidate the whole fleet's cache on
    a dev-only version bump as readily as on one that changes a span. The
    right-grained fix is pinning the handful of packages that actually
    participate in inference — gliner, onnxruntime, the tokenizer library
    — not the whole lockfile; left as a follow-up rather than done here.
    """
    root = root if root is not None else Path(__file__).parent
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*.py")):
        digest.update(path.relative_to(root).as_posix().encode("utf-8"))
        digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()


def detector_version(model_id: str) -> str:
    """`model_id` has no default deliberately: a forgotten argument here is
    exactly the bug this signature exists to make impossible to write by
    accident — silently falling back to the pinned constant regardless of
    what is actually loaded."""
    catalog_dir = resources.files("tessera_detector") / "catalog"
    blobs = [(catalog_dir / name).read_bytes() for name in CATALOGS]
    # Folded in as one more blob rather than string-concatenated onto
    # `model_id`: `version_from` already hashes each blob before folding it
    # in for exactly this reason (see its own comment) — appending the
    # digest as a string would reopen the same concatenation-boundary
    # question it exists to close.
    blobs.append(source_digest().encode("utf-8"))
    return version_from(model_id, blobs)


__all__ = ["CATALOGS", "detector_version", "source_digest", "version_from"]
