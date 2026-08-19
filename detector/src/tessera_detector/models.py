"""Where the NER weights live. They are never committed — hundreds of megabytes.

Lookup order is TESSERA_NER_MODEL, then the user cache. A variable pointing at a
missing path is an error: a typo must not quietly downgrade the pipeline to the
deterministic layer alone.
"""

import hashlib
import os
import re
from importlib import metadata
from pathlib import Path

MODEL_NAME = "gliner_multi-v2.1"
# The upstream `urchade/gliner_multi-v2.1` repo publishes PyTorch weights only.
# This mirror re-exports the same model with ONNX graphs (`onnx/model.onnx` and
# quantized variants), which is what `GlinerRecognizer` loads via onnxruntime.
HF_REPO_ID = f"onnx-community/{MODEL_NAME}"
# Pinned: the published metrics are only reproducible if the same Tessera commit
# always runs the same weights. Bump deliberately, and re-measure when you do.
HF_REVISION = "6ddaeb9413b0e71ad8457da1aab378a165b24058"
# The one ONNX graph GlinerRecognizer actually loads, named once here and
# referenced by both the loader (`ner.py`) and `weights_digest` below, so the
# two cannot drift apart the way `REQUIRED_ARTIFACTS` and "what the loader
# reads" already have once. The mirror ships two other quantizations
# (fp16, int8) alongside this one; neither is ever opened.
ONNX_MODEL_FILE = "onnx/model.onnx"
# What GlinerRecognizer needs to load, in the narrow sense of "did the
# download finish": snapshot_download creates the directory before it
# finishes, so directory existence alone would let an interrupted download
# masquerade as installed weights and crash the loader instead of falling
# back to the deterministic layer. This is deliberately not the same list
# `weights_digest` hashes — that answers "what determines the output",
# this answers "is it worth trying to load at all", and conflating them is
# how a version digest ended up covering less than the loader actually
# reads: the tokenizer files below are required for a correct load but are
# not dominant enough to matter for this fast completeness check.
REQUIRED_ARTIFACTS = (ONNX_MODEL_FILE, "config.json")
# Bookkeeping written by `snapshot_download`'s own resumable-download
# machinery — lock files and `*.metadata` sidecars that carry a download
# timestamp. Never read by the loader, and not reproducible across two
# downloads of byte-identical weights, so `weights_digest` excludes it: an
# identity that varied with when the weights happened to be fetched would
# be worse than the path-based one it replaces.
_DOWNLOAD_BOOKKEEPING_DIR = ".cache"
_HASH_CHUNK_SIZE = 1024 * 1024


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


def _hash_file(path: Path) -> bytes:
    # Chunked rather than `path.read_bytes()`: the largest artifact here is
    # over a gigabyte, `GlinerRecognizer` has usually already loaded it into
    # the ONNX runtime by the time this runs, and materializing the whole
    # file again as one `bytes` object would carry the graph twice in
    # memory at once — a container sized for the model alone can OOM on
    # that second copy. A fixed-size read loop never holds more than one
    # chunk.
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(_HASH_CHUNK_SIZE), b""):
            digest.update(chunk)
    return digest.digest()


def _weighed_files(path: Path) -> list[Path]:
    """Every file the loader can actually read, relative to `path`, sorted.

    Not `REQUIRED_ARTIFACTS`: that list answers a different, narrower
    question (see its own docstring above) and was never meant to enumerate
    everything that determines the model's output — `GLiNER.from_pretrained`
    reads the whole directory, tokenizer files included, and a version that
    only covers two of those files reports the pinned snapshot's identity
    for a tokenizer it did not use. Walking the directory instead means an
    artifact added later is picked up automatically, not missed until
    someone remembers to extend a second, separately maintained list.

    Two exclusions, both named because the alternative — hashing literally
    everything — was considered and rejected for each:
    `_DOWNLOAD_BOOKKEEPING_DIR` (see its docstring: not read by the loader,
    and not reproducible across identical downloads); and every ONNX graph
    except `ONNX_MODEL_FILE` (the mirror ships two more quantizations that
    `ner.py` never opens — hashing them would nearly double this function's
    dominant cost for a change that cannot affect a single span).
    """
    onnx_dir = path / "onnx"
    files = [
        candidate
        for candidate in path.rglob("*")
        if candidate.is_file()
        and _DOWNLOAD_BOOKKEEPING_DIR not in candidate.relative_to(path).parts
        and not (
            candidate.parent == onnx_dir
            and candidate != path / ONNX_MODEL_FILE
            and candidate.suffix == ".onnx"
        )
    ]
    return sorted(files, key=lambda candidate: candidate.relative_to(path).as_posix())


def weights_digest(path: Path) -> str:
    """A digest over the artifact bytes actually found at `path` — not the
    path itself, which `TESSERA_NER_MODEL` can point anywhere, and the same
    path can hold different bytes across a redeploy. This is what the
    version the gateway caches by has to name honestly: two detectors that
    both call themselves the pinned snapshot but load different weights
    (one overridden, one not) must not report the same version — and that
    has to hold for every file the loader reads, not only the two
    `REQUIRED_ARTIFACTS` happens to name; see `_weighed_files`.

    Called once, when the model is resolved — not per request. `onnx/model.onnx`
    alone runs over a gigabyte; hashing it on every `/detect` call would add
    real latency to the one feature (the gateway's cache) whose entire point
    is to avoid work on the common path. Measured warm-cache: ~0.75 s for the
    full graph, paid once at startup alongside the model load itself, which
    already costs "seconds" (see the top-level README).

    Raises whatever the read raises if a file cannot be read — deliberately
    not caught here. `find_model` has already confirmed the required
    artifacts exist by the time this runs, and the rest were just listed by
    the walk above; a read failure past that point means the weights'
    identity cannot be established, and reporting a version anyway would
    silently reintroduce the bug this function exists to close. Fail the
    startup instead of guessing.
    """
    digest = hashlib.sha256()
    for file in _weighed_files(path):
        digest.update(file.relative_to(path).as_posix().encode("utf-8"))
        digest.update(_hash_file(file))
    return digest.hexdigest()


# A distribution name from the start of a PEP 508 requirement string —
# "torch>=2.0.0" -> "torch", "onnxruntime-gpu; extra == \"gpu\"" ->
# "onnxruntime-gpu". Deliberately not the `packaging` library's own
# `Requirement` parser: that dependency is only ever present once the
# `ner` group is installed, and this pattern is all `dependency_digest`
# needs from the string — a name, not a validated specifier.
_REQUIREMENT_NAME = re.compile(r"^([A-Za-z0-9][A-Za-z0-9._-]*)")


def _requirement_name(requirement: str) -> str:
    match = _REQUIREMENT_NAME.match(requirement)
    if match is None:
        raise ValueError(f"cannot find a distribution name in requirement {requirement!r}")
    return match.group(1)


def _normalize_distribution_name(name: str) -> str:
    # PEP 503: distribution names compare case- and separator-insensitively
    # ("PyYAML" and "pyyaml", "typing_extensions" and "typing-extensions").
    # Without this, the same distribution reached by two different spellings
    # would be walked and hashed twice, and could each recurse into the
    # other's requirements as if they were unrelated.
    return re.sub(r"[-_.]+", "-", name).lower()


def _transitive_requirements(root: str) -> set[str]:
    """Every distribution `root` depends on, transitively, through its own
    declared requirements — not what happens to be imported in the current
    process, which depends on when in the process's life this runs (a
    second construction in one process finds everything already imported,
    so a diff against `sys.modules` reports nothing new for it — the
    defect this function replaces).

    Requirements gated behind an extra (`; extra == "gpu"`) are skipped:
    they describe optional features nothing here activates, not what a
    plain `pip install gliner` — which is what this project's own
    dependency group does — actually pulls in. Requirements gated behind
    other markers (`; sys_platform == "..."`) are not evaluated and may
    list a distribution this platform never installs; `dependency_digest`
    below tolerates that by skipping anything with no resolvable version,
    the same way it already tolerates a module with no distribution.
    """
    seen: set[str] = set()
    frontier = [root]
    while frontier:
        name = frontier.pop()
        normalized = _normalize_distribution_name(name)
        if normalized in seen:
            continue
        seen.add(normalized)
        try:
            requirements = metadata.requires(name) or ()
        except metadata.PackageNotFoundError:
            continue
        for requirement in requirements:
            if "extra ==" in requirement:
                continue
            dependency_name = _requirement_name(requirement)
            if _normalize_distribution_name(dependency_name) not in seen:
                frontier.append(dependency_name)
    return seen


def dependency_digest(root: str) -> str:
    """A digest over the installed versions of `root` and everything it
    depends on, transitively — declared in package metadata, not read from
    which modules this process happened to import.

    A rebuild that moves GLiNER, onnxruntime or the tokenizer library to a
    different release — weights, catalogs and this package's own sources
    all unchanged — can still change what a text detects to, and neither
    `weights_digest` nor the source digest in `version.py` would see it.
    Naming the packages by hand here would repeat the exact mistake
    `REQUIRED_ARTIFACTS` already made once in this file: correct today,
    silently under-covering the day someone adds a dependency GLiNER pulls
    in transitively. Walking the declared dependency tree instead means a
    new one is picked up the next time it appears in `gliner`'s own
    metadata (or that of anything it depends on), with nothing here to
    update by hand.

    This replaced an earlier version keyed on a `sys.modules` diff around
    the model load. That was a fact about process history, not about what
    is installed: a second `GlinerRecognizer` built in the same process —
    real in this suite, at `test_gliner.py`'s `strict = GlinerRecognizer(...)`
    — finds everything already imported, so the diff for it is empty and
    the two constructions report different identities for identical
    weights, code and dependencies. Reading `importlib.metadata.requires()`
    instead has no process to depend on: the same environment reports the
    same digest every time, which `test_models.py` pins directly.

    Boundary, stated rather than left implicit: this covers what `root`'s
    distribution *declares*, transitively. A package imported at runtime
    but never declared as a dependency — by `root` or anything in its
    tree — would be missed, the mirror image of the gap the `sys.modules`
    approach had. Nothing in GLiNER's own dependency tree does that today.
    """
    versions: dict[str, str] = {}
    for name in _transitive_requirements(root):
        try:
            versions[name] = metadata.version(name)
        except metadata.PackageNotFoundError:
            # Declared but not installed on this platform — a marker this
            # function does not evaluate (`; sys_platform == "..."`) ruled
            # it out, or an optional backend nothing here requires. Not a
            # version to miss: there is no installed package here to have
            # one.
            continue
    digest = hashlib.sha256()
    for name in sorted(versions):
        digest.update(f"{name}=={versions[name]}".encode())
    return digest.hexdigest()


__all__ = [
    "HF_REPO_ID",
    "HF_REVISION",
    "MODEL_NAME",
    "ONNX_MODEL_FILE",
    "REQUIRED_ARTIFACTS",
    "ModelUnavailable",
    "dependency_digest",
    "find_model",
    "model_cache_dir",
    "weights_digest",
]
