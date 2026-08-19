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

`catalog_text` is caller-supplied for the identical reason: `Detector`'s own
`catalog_text=` argument can replace identifiers.yaml with an application's
own rules, and this module has to digest the rules `DeterministicDetector`
actually parsed, not the package's packaged copy — `Detector.catalog_text`
is the honest value, the same relationship `Detector.model_id` already has
to the weights. `ner.yaml` has no such override anywhere in this codebase,
so it alone is still read from the package's own resources; see
`detector_version`'s own docstring for the boundary that leaves.
"""

import hashlib
import sys
import unicodedata
from collections.abc import Iterable
from importlib import resources
from pathlib import Path

# ner.yaml has no equivalent of Detector's `catalog_text=`: nothing in this
# codebase lets a caller replace it, so `detector_version` reads it from the
# package's own resources unconditionally — honest today because there is no
# other copy of it to be honest about. The day something adds an override
# for it too, it has to follow `catalog_text`'s path below rather than keep
# reading this file directly.
NER_CATALOG = "ner.yaml"


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


def interpreter_id() -> str:
    """The Python runtime's own identity: implementation, version, and the
    Unicode database it loaded.

    `normalize.py` calls `unicodedata.normalize("NFKC", ...)`, and
    `deterministic.py`'s rules match through the stdlib `re` engine over
    that normalized text — both against the same interpreter's own
    Unicode tables, sourced from neither the source tree (`source_digest`)
    nor any declared dependency (`models.dependency_digest`). A runtime
    whose Unicode database differs can change normalized text, matches and
    offsets with no source, catalog, model or distribution version moving.
    Real, not hypothetical, for this package specifically:
    `requires-python = ">=3.14"` is an open lower bound that permits any
    later interpreter, and neither of `Dockerfile`'s two base images
    (`ghcr.io/astral-sh/uv:python3.14-trixie-slim`,
    `python:3.14-slim-trixie`) is pinned to a digest — a rebuild can land
    on a different point release, or the same point release rebuilt
    against a newer Debian trixie image, with no change to this
    package's own files at all.

    Three fields, not one, because each protects a different mechanism
    that could vary independently:

    - `sys.implementation.name` — a different implementation entirely
      (PyPy, GraalPy) can carry its own `re` engine with its own matching
      semantics, unrelated to Unicode table version.
    - `sys.version_info`, in full rather than to the patch level — a
      prerelease carries the same first three fields as the final it
      precedes, so 3.15.0rc1, 3.15.0rc2 and 3.15.0 are one identity
      without `releaselevel` and `serial`, and `requires-python`'s open
      bound permits all three. `re`'s own behaviour has changed between
      CPython releases before, independent of the Unicode Character
      Database version, and a prerelease is exactly where it changes.
    - `unicodedata.unidata_version` — the fact the paragraph above is
      really about: two interpreters reporting the same
      `sys.version_info` are not guaranteed to load the same Unicode
      Character Database revision, and this is what `NFKC` actually
      normalizes against.

    See `detector_version` for the criterion this satisfies and the
    boundary of what it does not extend to.
    """
    major, minor, micro, releaselevel, serial = sys.version_info
    version = f"{major}.{minor}.{micro}.{releaselevel}.{serial}"
    return f"{sys.implementation.name}-{version}-unicode{unicodedata.unidata_version}"


def detector_version(model_id: str, catalog_text: str) -> str:
    """Neither argument has a default, deliberately: a forgotten argument
    here is exactly the bug this signature exists to make impossible to
    write by accident — silently falling back to the pinned constant, or to
    the package's own packaged catalog, regardless of what a `Detector`
    actually loaded.

    `catalog_text` is `Detector.catalog_text` — the identifiers.yaml text
    `DeterministicDetector` actually parsed, a caller's own catalog or the
    packaged default already resolved when none was given — not a second,
    independent read of the package's own copy. Passing the object's own
    value, the same way `model_id` already does for the weights, is what
    makes a caller-supplied catalog visible here at all: an application
    that overrides `catalog_text` gets detection from those rules, and this
    has to see that they changed. Seven inputs now follow this rule, one
    after another as each was found missing it: the pinned weights
    (`model_id`), the NER runtime's own dependencies, the deterministic
    layer's own dependencies (both folded into `model_id` by
    `pipeline.build_detector`), this package's own source (`source_digest`),
    the identifiers catalog, and now the interpreter itself
    (`interpreter_id`). Read what the object or the process holds, never
    re-read the package's own copy of it — the rule every one of them
    follows.

    That rule answers *how* an input gets in once it qualifies. It does
    not say what qualifies, and nine rounds of finding one more thing this
    digest did not cover is what happens without an answer to that
    question too — the interpreter is not the last thing a rebuild can
    change. The criterion, checked against every input above and below
    rather than declared from nothing: an input belongs in this digest
    when (1) it can actually change what a text detects to, and (2) that
    identity is available cheaply, deterministically and from inside this
    process — no shelling out, no parsing another program's output, no
    dependence on the machine's own state beyond what the process already
    loaded.

    Both conditions matter, not just the first. `sys.platform` fails
    condition (1) outright: this package touches nothing platform-specific
    in the paths that produce spans (grep the source — `normalize.py` and
    `deterministic.py` use only `unicodedata` and `re`, both of which ship
    their own tables rather than reading the OS's), so recording it would
    never actually distinguish two builds that detect differently.

    Three things that were considered and left out, because at least one
    condition fails for each — named here rather than left to the next
    reviewer to wonder whether they were missed:

    - The host C library. `unicodedata` and `re` use CPython's own
      bundled Unicode tables, not libc's — condition (1) is unverified at
      best for the operations this package actually performs, and no
      onnxruntime internals were audited to be sure a compiled dependency
      never touches it. Moot regardless: there is no portable stdlib way
      to name "the libc version" from inside a Python process across the
      platforms this package runs on, so condition (2) fails on its own.
    - The CPU features onnxruntime's kernels dispatch on (AVX2 vs
      AVX-512, say). Real: different dispatch can mean different
      floating-point paths through the same model. But the dispatch
      decision is internal to onnxruntime's own C++ runtime and is not
      exposed through a public API this process can read; querying the
      CPU's own advertised feature flags would only say what the hardware
      *could* dispatch to, not what a given kernel call actually did.
      Fails condition (2): not cheap, not from inside this process.
    - The ONNX execution provider a session actually loads with.
      Unlike the two above, this one is genuinely cheap, deterministic
      and in-process — `InferenceSession.get_providers()` answers it
      directly, so it does not fail either condition on its own terms.
      Left out because, today, it carries no information: this package's
      own `pyproject.toml` pins onnxruntime to a CPU build and never
      installs a GPU provider, so every session that loads at all reports
      the same one. The day a GPU provider becomes installable, that
      stops being true, and this is the input to add then — not a
      boundary that holds by definition, one that holds by this
      deployment's own present architecture, and the trigger for
      revisiting it is that architecture changing.

    What staying outside the boundary means, plainly: a difference this
    digest cannot see is a difference the gateway's cache cannot see
    either. A deployment whose fleet runs mixed libc versions, or whose
    hardware mix spans CPU generations old enough for kernel dispatch to
    differ, can serve spans from one host's cache to a request a
    different host's detector would have answered slightly differently.
    That is not a bug this digest failed to catch; it is the edge of what
    an in-process digest can ever see, stated so a deployment that cares
    can decide — pin the fleet to one image and one CPU baseline, or
    accept the gap — rather than discover it by noticing drift nobody
    can explain.
    """
    catalog_dir = resources.files("tessera_detector") / "catalog"
    ner_catalog_bytes = (catalog_dir / NER_CATALOG).read_bytes()
    # Folded in as blobs rather than string-concatenated onto `model_id`:
    # `version_from` already hashes each blob before folding it in for
    # exactly this reason (see its own comment) — appending anything as a
    # plain string would reopen the same concatenation-boundary question it
    # exists to close.
    blobs = [
        catalog_text.encode("utf-8"),
        ner_catalog_bytes,
        source_digest().encode("utf-8"),
        interpreter_id().encode("utf-8"),
    ]
    return version_from(model_id, blobs)


__all__ = ["NER_CATALOG", "detector_version", "interpreter_id", "source_digest", "version_from"]
