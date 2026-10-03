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

**Scope, so the green tick is not read as more than it is.** This checks the
numeric tables, the Article 9 coverage figure and the Tier 1 recall line. It
does not check prose counts such as "the eight remaining misses" or
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

ROOT = pathlib.Path(__file__).resolve().parent.parent
README = ROOT / "README.md"
CORPUS = ROOT / "evaluation" / "corpus" / "public.jsonl"

# `| PERSON | 0.968 | 0.803 | 0.878 |`, which is the only three-decimal row
# shape in the file; the surrounding tables are prose or timings.
ROW = re.compile(
    r"^\|\s*([A-Z][A-Z_0-9]*)\s*\|\s*(\d\.\d{3})\s*\|\s*(\d\.\d{3})\s*\|\s*(\d\.\d{3})\s*\|$"
)
COVERAGE = re.compile(r"\*\*Article 9 coverage is (\d\.\d{4}) \((\d+) of (\d+)\)\*\*")


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
    # The measurement names the corpus it came from, so a file left over from an
    # earlier corpus cannot carry this gate green — the one failure that would
    # look exactly like a pass.
    corpus = hashlib.sha256(CORPUS.read_bytes()).hexdigest()
    if measured.get("corpus_sha256") != corpus:
        print(
            f"FAIL: {path} measures corpus {measured.get('corpus_sha256')}, "
            f"but the corpus on disk is {corpus}. Re-run the measurement.",
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
        f"published metrics: {len(published)} rows and the Article 9 coverage "
        "figure match the corpus"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
