"""NER layer: zero-shot type configuration and the model adapter (REQ-1, REQ-4).

Types are data: entity type, the label handed to the model, its threshold, tier and
specificity all come from ner.yaml, so adding a type never touches this module.
"""

import copy
import math
import os
import warnings
from collections.abc import Iterable, Iterator, Mapping
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from importlib import resources
from pathlib import Path

import yaml

from .models import ONNX_MODEL_FILE, dependency_digest
from .spans import Span

_DEFAULT_CONFIG = resources.files("tessera_detector") / "catalog" / "ner.yaml"


@dataclass(frozen=True, slots=True)
class InferencePass:
    """One call to the model: the labels of a tier and that tier's floor."""

    tier: int
    labels: tuple[str, ...]
    threshold: float


@dataclass(frozen=True, slots=True)
class NerType:
    entity_type: str
    label: str
    threshold: float
    tier: int
    specificity: int
    # Words this type's spans may not begin with, lowercase and without a
    # trailing dot. Empty for every type but `PERSON`, which is the only one
    # the corpus shows over-capturing.
    trim_leading: frozenset[str] = frozenset()
    # Words trimmed only when another trimmable word follows. Articles: `Le` is
    # a family name in Vietnamese and `Das` in Bengali, so one on its own is not
    # evidence of anything.
    trim_leading_articles: frozenset[str] = frozenset()


def load_ner_types(config_text: str | None = None) -> tuple[NerType, ...]:
    if config_text is None:
        config_text = _DEFAULT_CONFIG.read_text(encoding="utf-8")
    config = yaml.safe_load(config_text)
    types: list[NerType] = []
    seen: set[str] = set()
    seen_labels: set[str] = set()
    for entry in config["entities"]:
        entity_type = entry["entity_type"]
        if entity_type in seen:
            raise ValueError(f"ner config declares {entity_type!r} twice")
        seen.add(entity_type)
        if "threshold" not in entry:
            raise ValueError(f"ner type {entity_type!r} declares no threshold")
        threshold = entry["threshold"]
        if (
            isinstance(threshold, bool)
            or not isinstance(threshold, int | float)
            or not 0.0 < threshold <= 1.0
        ):
            raise ValueError(
                f"ner type {entity_type!r} declares threshold {threshold!r} "
                "outside the (0.0, 1.0] range"
            )
        tier = entry["tier"]
        if isinstance(tier, bool) or not isinstance(tier, int) or not 1 <= tier <= 3:
            raise ValueError(f"ner type {entity_type!r} declares tier {tier!r} outside 1..3")
        label = entry["label"]
        if not isinstance(label, str) or not label.strip():
            raise ValueError(f"ner type {entity_type!r} declares an empty label")
        # The recognizer maps a model result back to its type by label, so a
        # shared label would make every earlier type undetectable.
        if label in seen_labels:
            raise ValueError(f"ner type {entity_type!r} reuses the label {label!r}")
        seen_labels.add(label)
        if "specificity" not in entry:
            raise ValueError(f"ner type {entity_type!r} declares no specificity")
        specificity = entry["specificity"]
        if isinstance(specificity, bool) or not isinstance(specificity, int) or specificity < 0:
            raise ValueError(
                f"ner type {entity_type!r} declares specificity {specificity!r} "
                "outside the non-negative integer range"
            )
        trim_leading = _word_list(entity_type, entry, "trim_leading")
        trim_leading_articles = _word_list(entity_type, entry, "trim_leading_articles")
        # An entry in both lists would be subtracted out of the article
        # safeguard and trimmed unconditionally — `le` in both makes
        # `Le Thi Mai` lose its family name, which is the disclosure the split
        # exists to prevent. A duplicate is a catalog mistake that disables a
        # safety rule, so it stops the service rather than the name.
        shared = trim_leading & trim_leading_articles
        if shared:
            raise ValueError(
                f"ner type {entity_type!r} lists {sorted(shared)!r} as both a trim_leading word "
                "and an article: an article is only trimmed ahead of another trimmable word, and "
                "listing it in both would trim it unconditionally."
            )
        types.append(
            NerType(
                entity_type=entity_type,
                label=label,
                threshold=float(threshold),
                tier=tier,
                specificity=specificity,
                trim_leading=trim_leading,
                trim_leading_articles=trim_leading_articles,
            )
        )
    return tuple(types)


# Boundaries to prefer when cutting, best first: a chunk that ends mid-entity
# costs a detection, so cuts land on paragraph, line or word breaks.
_BOUNDARIES = ("\n\n", "\n", " ")


def _word_list(entity_type: str, entry: dict[str, object], field: str) -> frozenset[str]:
    """A catalog list of single words, normalized, or an empty set.

    **One token each, refused at load time.** The rule walks a span a word at a
    time, so `Der Kunde` written as one entry could never match and would sit in
    the catalog looking as though it did — worse than an absent entry, because
    it reads as coverage.
    """
    words = entry.get(field, [])
    if not isinstance(words, list) or any(
        not isinstance(word, str) or not word.strip() for word in words
    ):
        raise ValueError(f"ner type {entity_type!r} declares {field} that is not a list of words")
    for word in words:
        if len(word.split()) != 1:
            raise ValueError(
                f"ner type {entity_type!r} declares the {field} entry {word!r}, which is more "
                "than one word: the rule matches a token at a time, so a phrase never matches. "
                "List its words separately."
            )
    return frozenset(_normalized(word) for word in words)


def _normalized(word: str) -> str:
    """A leading word as the list holds it: lowercase, without a trailing dot.

    So `Dr.`, `Dr` and `dr` are one entry rather than three, and a catalog that
    spells one of them is not quietly missing the others.
    """
    return word.strip().rstrip(".").casefold()


def _carries_a_word(token: str) -> bool:
    """Whether a token holds anything but punctuation."""
    return any(character.isalnum() for character in token)


def trim_leading_words(
    text: str,
    start: int,
    end: int,
    words: frozenset[str],
    articles: frozenset[str] = frozenset(),
    *,
    piece_starts_a_word: bool = False,
) -> tuple[int, int]:
    """`(start, end)` with listed words dropped from the front of the span.

    The model reads `Der Kunde Karz` as one person, so the same person behind a
    different role noun or title is a different *value* to a session that keys
    on equality — two placeholders for one person, which is the failure the
    README's stability argument is built against.

    **An article is trimmed only in front of another trimmable word.** `Le` is a
    Vietnamese family name and `Das` a Bengali one, so trimming a leading
    article on its own sends a real name component to the provider in clear:
    `Le Thi Mai` would arrive as `Le [PERSON_1]`. `Le salarié Gallet` still
    loses both words, because the article is followed by one that goes anyway.

    **Never trimmed to nothing.** A span made only of listed words is left as it
    came: a rule that can empty a span is a rule that can unmask a name, and a
    person really called `Herr` would be exactly that.

    **Never trimmed at all unless the span starts on a word boundary.** A token
    window can begin inside a word, and `der` at the end of `Alexander` is not
    an article — trimming there would expose the rest of a name the span was at
    least partly covering.

    Only the front: the corpus shows no trailing over-capture at all, and a rule
    trimming both ends would be twice the risk for none of the evidence.
    """
    if not words and not articles:
        return start, end
    at_document_boundary = text[start - 1].isspace() if start > 0 else piece_starts_a_word
    if not at_document_boundary:
        return start, end

    tokens: list[tuple[int, str]] = []
    at = start
    while at < end:
        while at < end and text[at].isspace():
            at += 1
        if at >= end:
            break
        token_start = at
        while at < end and not text[at].isspace():
            at += 1
        tokens.append((token_start, text[token_start:at]))

    trimmable = 0
    while trimmable < len(tokens) and _normalized(tokens[trimmable][1]) in (words | articles):
        trimmable += 1
    # An article at the end of the run is followed by the name, not by another
    # word that goes anyway, so it stays — and so does everything after it.
    while trimmable and _normalized(tokens[trimmable - 1][1]) in articles - words:
        trimmable -= 1
    # **What is left has to be a word, not merely a token.** The model puts
    # trailing punctuation inside a span often enough that `Herr !` reaches
    # here, and counting the `!` as remaining content let the trim keep the
    # punctuation and send `Herr` in clear. A span whose only survivors carry no
    # letter or digit is left as it came, which is the never-empty clause asked
    # about content rather than about count.
    if not trimmable or not any(_carries_a_word(token) for _, token in tokens[trimmable:]):
        return start, end
    return tokens[trimmable][0], end


def chunks(text: str, *, size: int, overlap: int) -> list[tuple[int, str]]:
    if size <= 0 or overlap < 0 or overlap >= size:
        raise ValueError(f"invalid chunk window: size={size!r}, overlap={overlap!r}")
    if not text:
        return []
    if len(text) <= size:
        return [(0, text)]
    result: list[tuple[int, str]] = []
    start = 0
    while start < len(text):
        end = min(start + size, len(text))
        if end < len(text):
            # Only accept a boundary in the last quarter of the window: cutting
            # too early would shrink chunks until progress stalls.
            floor = start + (size * 3) // 4
            for boundary in _BOUNDARIES:
                cut = text.rfind(boundary, floor, end)
                if cut != -1:
                    end = cut + len(boundary)
                    break
        result.append((start, text[start:end]))
        if end >= len(text):
            break
        start = max(end - overlap, start + 1)
    return result


# Character budgets for the first pass, with an overlap wide enough that a name
# split by one cut survives in the neighbouring chunk. Characters are only a
# proxy for tokens, so a second, token-exact pass bounds every chunk below.
CHUNK_SIZE = 1200
CHUNK_OVERLAP = 200
# Tokens the label prompt and the special tokens consume before any text does;
# subtracted from the model's window so a dense chunk cannot silently truncate.
PROMPT_TOKEN_RESERVE = 64
TOKEN_OVERLAP = 32

# **How many inferences one text may have in flight, and why it is half.**
#
# `detect` used to run its windows in a `for` loop, which left the machine idle
# on exactly the request this bound exists for: a large first-seen tool result
# is one caller waiting ninety seconds while ten cores do nothing (#28). ONNX
# releases the GIL, so dispatching the windows to threads is a real speedup, and
# it changes no answer — the seams were already handled by `chunks` and
# `token_windows` overlapping and `_spans_from` rebasing to absolute offsets.
#
# **Half the cores rather than all of them, because the first version starved
# small requests.** `api.detect` is a sync route, so Starlette already runs
# requests on threads; a shared pool plus `map` over every job queues a large
# document's two hundred inferences ahead of whatever arrives next. Measured,
# one large request against four small ones arriving after it:
#
#   serial               large 5.6s   small median 0.47s   worst 0.52s
#   all jobs at once     large 4.0s   small median 3.31s   worst 3.63s
#   a window of cores    large 4.4s   small median 1.43s   worst 1.87s
#   a window of cores/2  large 4.4s   small median 0.50s   worst 0.63s
#
# So the naive version buys a third off the large request by making every small
# one seven times slower, which is the wrong trade for a gateway whose ordinary
# traffic is chat messages. Bounding a single text to half the workers leaves a
# newcomer waiting for one inference rather than a document, and still gives a
# lone large request **1.45x** — against 1.63x for the unfair version.
#
# Every earlier measurement of this used requests of one size and could not see
# any of it.
# Where the kernel records this process's cgroup and where the hierarchies are
# mounted. Both are read, because neither alone locates the quota files.
_PROC_SELF_CGROUP = Path("/proc/self/cgroup")
_PROC_SELF_MOUNTINFO = Path("/proc/self/mountinfo")
# The override, for a deployment that knows better than any of the guesses below.
_WORKERS_ENV = "TESSERA_DETECT_WORKERS"


def _cpu_hierarchies() -> list[tuple[Path, str]]:
    """Every mounted cgroup hierarchy carrying a CPU limit: (mountpoint, mount root).

    **The mountpoint is not `/sys/fs/cgroup/cpu`, and guessing it was the fourth
    wrong answer here.** cgroup v1 usually mounts the controller *combined* —
    `/sys/fs/cgroup/cpu,cpuacct` — and a mount can expose a subtree rather than
    the whole hierarchy, in which case the path from `/proc/self/cgroup` has to
    be taken relative to that subtree before it means anything on disk. Neither
    is guessable; both are in `/proc/self/mountinfo`. Found in review of #63.

    A mountinfo line is

        id parent major:minor ROOT MOUNTPOINT options… - FSTYPE source SUPEROPTIONS

    where the separator is a bare `-`, so the optional fields before it are
    skipped by finding it rather than by counting.
    """
    hierarchies: list[tuple[Path, str]] = []
    try:
        lines = _PROC_SELF_MOUNTINFO.read_text().splitlines()
    except OSError:
        return hierarchies
    for line in lines:
        try:
            before, after = line.split(" - ", 1)
        except ValueError:
            continue
        left = before.split()
        right = after.split()
        if len(left) < 5 or len(right) < 3:
            continue
        mount_root, mountpoint = left[3], left[4]
        fs_type, super_options = right[0], right[2]
        if fs_type == "cgroup2":
            hierarchies.append((Path(mountpoint), mount_root))
        elif fs_type == "cgroup" and "cpu" in super_options.split(","):
            # `cpu,cpuacct` is one mount answering to both names, and the
            # option list is where it says so.
            hierarchies.append((Path(mountpoint), mount_root))
    return hierarchies


def _cgroup_paths() -> list[Path]:
    """Every directory that could carry a CPU limit for this process.

    **The process is usually not at the hierarchy root, and the first version
    read the root.** A cgroup-v1 container sharing the host's namespace, or any
    systemd-managed service, lives at a path recorded in `/proc/self/cgroup` —
    `/system.slice/tessera.service`, `/docker/9f2c…`. Reading the root inspects
    something unlimited, so the quota came back `None` and the pool was sized
    from the affinity count again.

    **Ancestors are included, because a limit on a parent slice binds too.** A
    service under a `.slice` capped at two CPUs is capped at two whatever its
    own directory says, so every level is a candidate and `_cgroup_cpu_quota`
    takes the narrowest.
    """
    hierarchies = _cpu_hierarchies()
    if not hierarchies:
        return []
    try:
        lines = _PROC_SELF_CGROUP.read_text().splitlines()
    except OSError:
        return [mountpoint for mountpoint, _ in hierarchies]
    relative: list[str] = []
    for line in lines:
        fields = line.split(":", 2)
        if len(fields) != 3:
            continue
        hierarchy, controllers, path = fields
        # v2 is the line with an empty controller list and hierarchy `0`; v1
        # gives one line per controller and only a `cpu` one matters here.
        if (hierarchy == "0" and not controllers) or "cpu" in controllers.split(","):
            relative.append(path)

    paths: list[Path] = []
    for mountpoint, mount_root in hierarchies:
        paths.append(mountpoint)
        for path in relative:
            # A mount may expose a subtree. The cgroup path is relative to the
            # hierarchy; on disk it is relative to what this mount shows of it.
            if mount_root != "/":
                if not path.startswith(mount_root):
                    continue
                path = path[len(mount_root) :]
            here = mountpoint
            for part in path.strip("/").split("/"):
                if part:
                    here = here / part
                    paths.append(here)
    return paths


def _quota_at(directory: Path) -> float | None:
    """The CPU limit written in one cgroup directory, in CPUs, or `None`."""
    try:
        quota, period = (directory / "cpu.max").read_text().split()[:2]
        if quota != "max" and float(period) > 0:
            return float(quota) / float(period)
        return None
    except (OSError, ValueError, IndexError):
        pass
    try:
        v1_quota = float((directory / "cpu.cfs_quota_us").read_text().strip())
        v1_period = float((directory / "cpu.cfs_period_us").read_text().strip())
        if v1_quota > 0 and v1_period > 0:
            return v1_quota / v1_period
    except (OSError, ValueError):
        pass
    return None


def _cgroup_cpu_quota() -> float | None:
    """CPUs this container may use, from its cgroup, or `None` if unlimited.

    **A quota is not an affinity mask, and this is the difference that matters.**
    `os.process_cpu_count()` reports `sched_getaffinity`, which a `cpuset`
    changes and `docker --cpus` / a Kubernetes CPU *limit* do not — those write
    a quota. So a container given two CPUs on a large host still sees the host's
    count there, builds a pool for it, and lets one document enqueue far more
    concurrent inferences than it can run. Confirmed in review, in an
    environment with a two-CPU quota where `process_cpu_count()` returned three.

    Both cgroup versions, every ancestor of the process's own path, narrowest
    wins. Anything unreadable or unparseable contributes nothing — a wrong guess
    here should cost the *default* sizing, never an exception at import.
    """
    quotas = [quota for path in _cgroup_paths() if (quota := _quota_at(path)) is not None]
    return min(quotas) if quotas else None


def _pool_size() -> int:
    """How many inferences this process may usefully have running.

    Three answers, narrowest wins, and the deployment's own beats all of them:

    - `os.cpu_count()` — the *machine*. Wrong for any container, and the first
      version of this used it: two CPUs on a 64-core host would have built 64
      workers and let one document enqueue 32 inferences onto two;
    - `os.process_cpu_count()` — the affinity mask. Right for a `cpuset`, still
      wrong for a quota, which is what `docker --cpus` and a Kubernetes CPU
      limit actually write;
    - the cgroup quota — right for those, absent on a bare host;
    - `TESSERA_DETECT_WORKERS` — because none of the above can know that this
      process shares its CPUs with something the kernel is not telling us about.

    Both findings that produced this came from review of #62 and #63, and both
    were about the same mistake in different clothes: asking a question whose
    answer describes something other than what the process may do.
    """
    override = os.environ.get(_WORKERS_ENV)
    if override is not None:
        # **Present-and-empty is malformed, not absent.** A Compose or Kubernetes
        # variable that expands to nothing is a deployment that tried to set a
        # cap and produced no cap, which is exactly the case where falling back
        # to a much larger automatic pool in silence is worst. Found in review
        # of #63, against a `if override:` that read "" as "unset".
        try:
            return max(1, int(override))
        except ValueError:
            # A malformed override is a deployment mistake, and guessing past it
            # silently is how it stays one. Named, then ignored.
            warnings.warn(
                f"{_WORKERS_ENV}={override!r} is not usable as a worker count; "
                "sizing from this process's own CPUs instead",
                RuntimeWarning,
                stacklevel=2,
            )
    allowed = float(os.process_cpu_count() or 1)
    quota = _cgroup_cpu_quota()
    if quota is not None:
        allowed = min(allowed, quota)
    # **Rounded up, not down.** A worker consumes at most one CPU, so a
    # container entitled to 1.5 of them and given one worker can never use the
    # half — and every other request queues behind that one worker for CPU time
    # the cgroup was willing to grant. Found in review of #63.
    return max(1, math.ceil(allowed))


def _in_flight(pool_size: int) -> int:
    """How many of the pool one text may occupy.

    **Half, and floored at one rather than two.** A floor of two on a
    single-CPU deployment starts two CPU-heavy inferences on one core and
    occupies every slot with them, adding contention exactly where there is
    least to absorb it. One is the correct degradation: it gives up the
    parallelism, and on one CPU there was none to give. Found in review of #63,
    against a comment of mine calling a bound of one "a `for` loop with extra
    steps" — which is precisely what it should be there.

    A function rather than an expression so the invariant that matters — never
    more in flight than there are workers — is checkable without a machine of
    the right size to check it on. That is how the floor of two survived: it is
    wrong only at one CPU, and no test ran on one.
    """
    return max(1, pool_size // 2)


_POOL_SIZE = _pool_size()
_IN_FLIGHT = _in_flight(_POOL_SIZE)
# Created once for the process and shared by every recognizer in it, which is
# what keeps the bound a bound: a pool per request is how oversubscription gets
# in. `concurrent.futures` joins these at interpreter exit, so there is nothing
# to shut down by hand.
_INFERENCE_POOL = ThreadPoolExecutor(max_workers=_POOL_SIZE, thread_name_prefix="detect")


def token_windows(
    offsets: list[tuple[int, int]], *, budget: int, overlap: int
) -> list[tuple[int, int]]:
    """Group token offsets into (char_start, char_end) windows of at most budget tokens.

    Character counts only approximate token counts: dense text can exceed the
    model's window inside one chunk, and inference then truncates the tail
    silently — the characters between the truncation point and the next chunk's
    start would never be looked at.
    """
    if budget <= 0 or overlap < 0 or overlap >= budget:
        raise ValueError(f"invalid token window: budget={budget!r}, overlap={overlap!r}")
    if not offsets:
        return []
    windows: list[tuple[int, int]] = []
    start = 0
    while start < len(offsets):
        end = min(start + budget, len(offsets))
        windows.append((offsets[start][0], offsets[end - 1][1]))
        if end >= len(offsets):
            break
        start = max(end - overlap, start + 1)
    return windows


class GlinerRecognizer:
    def __init__(self, model_path: Path, types: tuple[NerType, ...] | None = None) -> None:
        # Imported lazily: the base install does not carry the ner group, and
        # `import tessera_detector.ner` must keep working without it.
        from gliner import GLiNER

        self.model_path = model_path
        self.types = types or load_ner_types()
        self.specificity: Mapping[str, int] = {t.entity_type: t.specificity for t in self.types}
        self._by_label = {t.label: t for t in self.types}
        # The weights come from the onnx-community mirror (see models.HF_REPO_ID):
        # the upstream urchade repo ships PyTorch weights only. The mirror's ONNX
        # graph lives under onnx/model.onnx rather than at the repo root, so the
        # default onnx_model_file="model.onnx" must be overridden to match.
        # ONNX_MODEL_FILE is the same name models.weights_digest hashes as
        # "the one graph actually loaded" — one constant, not two lists that
        # can drift.
        self._model = GLiNER.from_pretrained(
            str(model_path), load_onnx_model=True, onnx_model_file=ONNX_MODEL_FILE
        )
        # "gliner" here is the PyPI distribution name (unrelated to
        # MODEL_NAME, which names the weights), the root `dependency_digest`
        # walks transitively through installed package metadata — not a
        # snapshot of what this process happened to import, which a second
        # construction here would find already loaded. See its own
        # docstring for what that distinction fixes.
        self.dependency_digest = dependency_digest("gliner")
        # **A copy, and the copy is the fix.** This used to be the very object
        # GLiNER tokenizes with, and sharing it makes the service fail under
        # concurrent requests: `api.detect` is a sync route, so Starlette runs
        # it in a threadpool, and two requests then use one HuggingFace fast
        # tokenizer from two threads.
        #
        # The failure is not a data race in our code but a `RuntimeError:
        # Already borrowed` out of the Rust object underneath. `transformers`
        # calls `set_truncation_and_padding` before each encode, which *mutates*
        # the tokenizer when the strategy changes — and the two callers here
        # want different strategies. `_windows` asks for offset mappings with no
        # padding; GLiNER pads for batching. So each call flips the state back,
        # every flip is a mutable borrow, and a concurrent reader panics.
        #
        # Neither caller races with itself: repeat calls in the same strategy
        # skip the mutation entirely. It takes the two *interleaved*, which is
        # what `detect` does, and is why isolating either one found nothing.
        # Measured through the real app before the copy: 11 of 64 requests
        # failed at eight concurrent, and 22 of 128 at sixteen.
        #
        # A lock is the other repair and it is worse: the borrow that panics is
        # inside GLiNER's inference, so the lock would have to cover the
        # inference, which serializes the only expensive part of the call.
        # Separate objects make the question not arise.
        #
        # The copy costs **26.5 MB against the model's 2 948 MB** — nine parts
        # in a thousand, measured rather than waved at, because "just copy it"
        # deserves a number when the thing being copied carries a 250 000-token
        # vocabulary.
        #
        # And it was the only shared mutable thing: inference against inference,
        # windowing against windowing, and whole `detect` calls against each
        # other all run clean on four threads once these two are apart.
        self._tokenizer = copy.deepcopy(self._model.data_processor.transformer_tokenizer)
        self._token_budget = int(self._model.config.max_len) - PROMPT_TOKEN_RESERVE
        # One inference pass per tier. GLiNER gives a span a single label, so
        # tiers competing in one call lose data: "ver.di" is claimed by
        # `organization` at 0.505 — beating `trade union` at 0.445 — and is then
        # dropped by ORG's own 0.75 threshold, so an Article 9 mention vanishes
        # because a quasi-identifier won the argmax and then failed its bar.
        # Separate passes cost one inference per tier and keep the categories
        # from bidding against each other.
        by_tier: dict[int, list[NerType]] = {}
        for ner_type in self.types:
            by_tier.setdefault(ner_type.tier, []).append(ner_type)
        self.passes = tuple(
            InferencePass(
                tier=tier,
                labels=tuple(t.label for t in group),
                threshold=min(t.threshold for t in group),
            )
            for tier, group in sorted(by_tier.items())
        )

    def _windows(self, chunk: str) -> list[tuple[int, int]]:
        encoded = self._tokenizer(chunk, return_offsets_mapping=True, add_special_tokens=False)
        offsets = [(int(s), int(e)) for s, e in encoded["offset_mapping"] if e > s]
        return token_windows(offsets, budget=self._token_budget, overlap=TOKEN_OVERLAP)

    def windows(self, text: str) -> Iterator[tuple[int, str]]:
        """Every piece handed to the model, with its absolute offset in `text`.

        A generator, not a list: the pieces are a second copy of the document
        plus its window overlap, and the CLI accepts files of any size. Callers
        that genuinely need them all — a benchmark timing one pass over fixed
        input — can materialize them and pay for it deliberately.
        """
        for offset, chunk in chunks(text, size=CHUNK_SIZE, overlap=CHUNK_OVERLAP):
            for start, end in self._windows(chunk):
                yield offset + start, chunk[start:end]

    def _spans_from(
        self, base: int, piece: str, inference: InferencePass, *, at_boundary: bool = False
    ) -> Iterator[Span]:
        for entity in self._model.predict_entities(
            piece, list(inference.labels), threshold=inference.threshold
        ):
            ner_type = self._by_label.get(entity["label"])
            score = float(entity["score"])
            if ner_type is None or score < ner_type.threshold:
                continue
            # A window can begin inside a source word, and this function is
            # handed the window rather than the document — so a span at offset
            # zero looks like a word boundary when it is the tail of
            # `Alexander`. `detect` knows better and says so; anything that
            # does not know says nothing and no trimming happens.
            start, end = trim_leading_words(
                piece,
                int(entity["start"]),
                int(entity["end"]),
                ner_type.trim_leading,
                ner_type.trim_leading_articles,
                piece_starts_a_word=at_boundary,
            )
            yield Span(
                entity_type=ner_type.entity_type,
                start=base + start,
                end=base + end,
                confidence=min(score, 0.99),
                recognizer="ner:gliner",
                tier=ner_type.tier,
            )

    def run_pass(
        self, pieces: Iterable[tuple[int, str]], inference: InferencePass
    ) -> list[Span]:
        """Spans from one inference pass over already-prepared pieces."""
        return [span for base, piece in pieces for span in self._spans_from(base, piece, inference)]

    def detect(self, text: str) -> list[Span]:
        if not text:
            return []

        def one(job: tuple[int, str, InferencePass, bool]) -> list[Span]:
            base, piece, inference, at_boundary = job
            return list(self._spans_from(base, piece, inference, at_boundary=at_boundary))

        spans: list[Span] = []
        batch: list[tuple[int, str, InferencePass, bool]] = []

        def drain() -> None:
            # `map` keeps submission order, so the result is the same list the
            # `for` loop produced — the same order, not merely the same set.
            for found in _INFERENCE_POOL.map(one, batch):
                spans.extend(found)
            batch.clear()

        # **Consumed in batches rather than collected.** `windows` is a
        # generator on purpose: the pieces are a second copy of the document
        # plus its overlap, and the CLI accepts files of any size. Materializing
        # them to hand the pool one list would give that up for a speedup on the
        # very inputs where it matters most.
        for base, piece in self.windows(text):
            at_boundary = base == 0 or text[base - 1].isspace()
            for inference in self.passes:
                batch.append((base, piece, inference, at_boundary))
                # **Inside the pass loop, not after it.** Draining only between
                # windows lets the batch reach `_IN_FLIGHT + passes - 1` before
                # anyone looks, so the cap this exists to enforce was advertised
                # and not applied: at two passes and a limit of three, one
                # document submitted four. On a small process that is the whole
                # pool, which is the case the bound is for. Found in review
                # of #62.
                if len(batch) >= _IN_FLIGHT:
                    drain()
        if batch:
            drain()
        return spans


__all__ = [
    "GlinerRecognizer",
    "InferencePass",
    "NerType",
    "chunks",
    "load_ner_types",
    "token_windows",
]
