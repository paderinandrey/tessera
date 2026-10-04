"""Fail when the figures the README publishes and the ones `evaluate.py`
measures disagree.

The README carries its numbers as literal Markdown, because a figure a reader
has to run a model to see is not published. That copy is only safe while
something notices it drifting, and until this script nothing did: the `PERSON`
row read `1.000 / 0.855 / 0.922` for weeks while the corpus measured
`0.985 / 0.882 / 0.931`, `docs/latency.md` carried the correct figures the whole
time, and CI was green over both. The drift was found by a reviewer reading the
two documents against each other, which is not a gate.

It reads the measurement from `evaluate.py --json` rather than recomputing it,
so there is one measurement and the gate cannot disagree with the run it is
checking.

**And it refuses a measurement that did not come from this tree.** The file
names the corpus it read and the `detector_version` of the detector that read
it, because a figure measured by something else passing as a figure measured
here is the one failure that looks exactly like a success. `evaluate.py` also
unlinks the file at the start of every run, so a run that does not measure
leaves none behind; the identities cover a file carried in from elsewhere,
which unlinking cannot see.

`detector_version` is the detector's own answer to "what determines my output"
rather than a second one invented here — the weights that actually loaded, the
NER and deterministic dependency digests, both catalogs, the package source and
the interpreter. The first version of this check hashed a source tree itself and
recorded `HF_REVISION`, which `version.py` had already explained is the wrong
value: the constant names the pinned snapshot, not the weights
`TESSERA_NER_MODEL` may have loaded instead. Reviewers on #99 pointed at the
duplication and at what it did not cover.

Imported rather than read textually, so this runs under `uv run --project
detector` like `check-entity-types` does. The detector it builds is
deterministic-only, so the gate still needs no weights.

**Scope, so the green tick is not read as more than it is.** This checks the
numeric tables, the Article 9 coverage figure, and the Tier 1 recall gate — both
that the threshold README publishes is the one `evaluate.py` enforces and that
the measurement clears it. It does not check prose counts such as
"the eight remaining misses" or
`docs/evaluation.md`'s miss inventory: splitting the entities that reach the
provider into real defects and annotation conventions is a judgement recorded
per entry in `KNOWN_UNMASKED`, not a number this script can derive. Those
counts are still only as right as the person who last edited them.
"""

import hashlib
import json
import pathlib
import re
import sys

from tessera_detector.models import dependency_digest, model_cache_dir
from tessera_detector.pipeline import PACKAGE_NAME, build_detector, ner_model_id
from tessera_detector.version import detector_version

ROOT = pathlib.Path(__file__).resolve().parent.parent
README = ROOT / "README.md"
CORPUS = ROOT / "evaluation" / "corpus" / "public.jsonl"
EVALUATOR = ROOT / "evaluation" / "evaluate.py"


# `| PERSON | 0.968 | 0.803 | 0.878 |`, which is the only three-decimal row
# shape in the file; the surrounding tables are prose or timings.
ROW = re.compile(
    r"^\|\s*([A-Z][A-Z_0-9]*)\s*\|\s*(\d\.\d{3})\s*\|\s*(\d\.\d{3})\s*\|\s*(\d\.\d{3})\s*\|$"
)
COVERAGE = re.compile(r"\*\*Article 9 coverage is (\d\.\d{4}) \((\d+) of (\d+)\)\*\*")
# `make evaluate   # ... + the Tier 1 recall gate (>= 0.99)`
TIER1 = re.compile(r"Tier 1 recall gate \(>= (\d\.\d+)\)")



def expected_model_id() -> str:
    """What the weights identity has to be, established here rather than read
    out of the file being checked.

    Composed by `pipeline.ner_model_id`, the same function `build_detector`
    uses, so the two cannot drift. Needs the weights on disk and the `ner`
    group installed — both true wherever a measurement could have been taken —
    and needs no inference session: file hashes and installed metadata only.
    """
    return ner_model_id(
        model_cache_dir(), dependency_digest(PACKAGE_NAME), dependency_digest("gliner")
    )


def published_rows(text: str) -> dict[str, tuple[str, str, str]]:
    rows = {}
    for line in text.splitlines():
        match = ROW.match(line.strip())
        if match is None:
            continue
        entity_type, precision, recall, f1 = match.groups()
        if entity_type in rows:
            raise SystemExit(f"README publishes {entity_type} twice")
        rows[entity_type] = (precision, recall, f1)
    return rows


def main() -> int:
    if len(sys.argv) != 2:
        print(
            "usage: check_published_metrics.py METRICS.json\n"
            "  produced by: evaluate.py --require-ner --json METRICS.json",
            file=sys.stderr,
        )
        return 2
    path = pathlib.Path(sys.argv[1])
    if not path.is_file():
        # Written only once the NER gates have run, so a missing file means the
        # measurement never happened. Passing here would make this gate green
        # on no evidence at all.
        print(
            f"FAIL: no measurement at {path}. It is written by\n"
            f"  evaluate.py --require-ner --json {path}\n"
            "which needs the NER weights (`make model`); a run without them "
            "writes nothing, because figures measured with the model off are "
            "not the published ones.",
            file=sys.stderr,
        )
        return 1
    measured = json.loads(path.read_text(encoding="utf-8"))
    # The measurement names what produced it, so a file from an earlier corpus
    # or an earlier model cannot carry this gate green — the one failure that
    # would look exactly like a pass. `evaluate.py` also deletes the file at the
    # start of every run, so a run that does not measure leaves none behind;
    # these checks cover a file carried in from a different tree.
    corpus = hashlib.sha256(CORPUS.read_bytes()).hexdigest()
    if measured.get("corpus_sha256") != corpus:
        print(
            f"FAIL: {path} measures corpus {measured.get('corpus_sha256')}, "
            f"but the corpus on disk is {corpus}. Re-run the measurement.",
            file=sys.stderr,
        )
        return 1
    evaluator = hashlib.sha256(EVALUATOR.read_bytes()).hexdigest()
    if measured.get("evaluator_sha256") != evaluator:
        # `detector_version` covers the detector, not the script that turns its
        # spans into the published figures: the Article 9 type list, the tier
        # selection and the aggregation all live in `evaluate.py` and all move a
        # number without moving the detector.
        print(
            f"FAIL: {path} was written by evaluator "
            f"{measured.get('evaluator_sha256')}, but evaluate.py is {evaluator}. "
            "Re-run the measurement.",
            file=sys.stderr,
        )
        return 1
    try:
        wanted_model = expected_model_id()
    except Exception as error:
        print(
            f"FAIL: cannot establish the expected weights identity: {error}. "
            "This needs the pinned weights (`make model`) and the ner group, "
            "which any run that could have produced a measurement already had.",
            file=sys.stderr,
        )
        return 1
    if measured.get("model_id") != wanted_model:
        print(
            f"FAIL: {path} was measured with weights {measured.get('model_id')}, "
            f"but this tree pins {wanted_model}. Figures produced through a "
            "`TESSERA_NER_MODEL` override are not the published ones.",
            file=sys.stderr,
        )
        return 1
    # Composed from the identity established above rather than the recorded one,
    # so nothing in this comparison comes from the file being checked.
    expected = detector_version(wanted_model, build_detector(ner=False).catalog_text)
    if measured.get("detector_version") != expected:
        print(
            f"FAIL: {path} was measured by detector {measured.get('detector_version')}, "
            f"but this tree is {expected}. A threshold, a rule, a catalog or the "
            "interpreter changed, which moves the figures without moving the "
            "corpus. Re-run the measurement.",
            file=sys.stderr,
        )
        return 1
    text = README.read_text(encoding="utf-8")
    published = published_rows(text)
    failures = []

    if not published:
        print("FAIL: no per-type rows found in README.md", file=sys.stderr)
        return 1

    for entity_type, (precision, recall, f1) in sorted(published.items()):
        if entity_type not in measured["per_type"]:
            failures.append(
                f"README publishes {entity_type}, which the corpus does not measure"
            )
            continue
        m = measured["per_type"][entity_type]
        want = (f"{m['precision']:.3f}", f"{m['recall']:.3f}", f"{m['f1']:.3f}")
        if (precision, recall, f1) != want:
            failures.append(
                f"{entity_type}: README publishes {precision} / {recall} / {f1}, "
                f"corpus measures {want[0]} / {want[1]} / {want[2]}"
            )

    # Named rather than counted, the same argument `KNOWN_UNMASKED` makes: a
    # count lets a row the README dropped pay for a row it invented.
    unpublished = sorted(set(measured["per_type"]) - set(published))
    if unpublished:
        failures.append(
            "the corpus measures types the README does not publish: "
            + ", ".join(unpublished)
        )

    tier1 = TIER1.search(text)
    if tier1 is None:
        failures.append("no Tier 1 recall gate sentence found in README.md")
    else:
        target = measured["targets"]["tier1_recall"]
        if float(tier1.group(1)) != target:
            failures.append(
                f"Tier 1 recall gate: README publishes >= {tier1.group(1)}, "
                f"evaluate.py requires >= {target}"
            )
        if measured["tier1_recall"] < target:
            failures.append(
                f"Tier 1 recall {measured['tier1_recall']} is below the published "
                f"target {target}"
            )

    coverage = COVERAGE.search(text)
    if coverage is None:
        failures.append("no Article 9 coverage sentence found in README.md")
    else:
        ratio, covered, gold = coverage.groups()
        want = measured["article_9_coverage"]
        if (ratio, int(covered), int(gold)) != (
            f"{want['ratio']:.4f}",
            want["covered"],
            want["gold"],
        ):
            failures.append(
                f"Article 9 coverage: README publishes {ratio} ({covered} of {gold}), "
                f"corpus measures {want['ratio']:.4f} "
                f"({want['covered']} of {want['gold']})"
            )

    for failure in failures:
        print(f"FAIL: {failure}", file=sys.stderr)
    if failures:
        print(
            f"\n{len(failures)} published figure(s) disagree with the corpus. "
            "Re-record them rather than relaxing this check: the numbers are the "
            "claim this repository makes about itself.",
            file=sys.stderr,
        )
        return 1
    print(
        f"published metrics: {len(published)} per-type rows, the Article 9 coverage "
        "figure and the Tier 1 recall gate match a measurement of this corpus, "
        f"taken by detector {expected[:12]}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
