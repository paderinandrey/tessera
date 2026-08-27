"""Fail when the gateway's entity-type vocabulary and the detector's catalogs
disagree.

The gateway holds its own copy on purpose — asking the detector which types it
emits would be worthless against a detector that lies. The copy is only safe
while something notices it drifting, and that is this script.

It checks the split as well as the total. `DETERMINISTIC_TYPES` must be
`identifiers.yaml` exactly, and the rest of `ENTITY_TYPES` must be `ner.yaml`
exactly, because the gateway refuses a request over a span in a *numeric* leaf
only when the type is a deterministic one. Comparing the union alone — which is
all this script did until the numeric refusal narrowed — would let a ninth
identifier land on the NER side of that predicate without anyone deciding it.
"""

import pathlib
import re
import sys

import yaml

ROOT = pathlib.Path(__file__).resolve().parent.parent
MAPPING = ROOT / "gateway" / "src" / "mapping.rs"
IDENTIFIERS = (
    ROOT / "detector" / "src" / "tessera_detector" / "catalog" / "identifiers.yaml"
)
NER = ROOT / "detector" / "src" / "tessera_detector" / "catalog" / "ner.yaml"

# One array element, alone on its line: `    "PERSON",`, with an explanatory
# `//` after it allowed — that is ordinary Rust and turning it into a CI failure
# about a vocabulary that has not drifted teaches people to distrust this check.
# The shape of the entry is matched rather than the shape of a name so that
# `ARTICLE9` — a name the placeholder grammar would not admit — is seen and
# reported rather than silently skipped as unparsable.
ENTRY = re.compile(r'^\s*"([^"]*)",\s*(?://.*)?$', re.MULTILINE)


def rust_array(source: str, name: str) -> set[str]:
    """The names in one `pub const NAME: [&str; n]` array in `mapping.rs`."""
    # Rust defines each of these constants once. A second copy of a declaration
    # is therefore either commented out or dead, and this script cannot tell
    # which of the two the compiler reads — it reads the first. A commented-out
    # copy carrying the names as they used to be would be read here in place of
    # the array below it, and unlike a commented-out *entry* the length check
    # cannot notice, because a copy brings its own length. Counting the
    # definition is what closes that, and it needs no theory of comments either.
    if source.count(f"pub const {name}") != 1:
        sys.exit(
            f"{MAPPING}: {name} is written "
            f"{source.count(f'pub const {name}')} times. Only one of them "
            "is the array the gateway compiles, and this check cannot tell which."
        )
    match = re.search(
        rf"pub const {name}: \[&str; (\d+)\] = \[(.*?)\];", source, re.DOTALL
    )
    if match is None:
        sys.exit(f"{MAPPING}: no {name} declaration found")
    declared, entries = int(match.group(1)), ENTRY.findall(match.group(2))
    # The array's own length, against the entries read out of it. Three versions
    # of this check in a row were defeated by a way of hiding a name that nobody
    # had listed — a `//` comment quoting one, then a one-line `/* */`, then a
    # `/* */` spanning lines with the entry inside it — and each fix answered
    # only the example it was given. This one needs no list: the length is in
    # the type and rustc holds it to the elements, so a name that only one of
    # the two readers can see is a miscount here, whatever hid it. Nothing below
    # depends on knowing what a comment looks like.
    #
    # The anchor is the written length. Were either array ever to become a slice
    # with no length in its type, the pattern above stops matching and this
    # script exits saying so, rather than quietly losing half its guarantee.
    if len(entries) != declared:
        sys.exit(
            f"{MAPPING}: {name} is declared with {declared} entries and "
            f"{len(entries)} were read from it. Either an entry is commented out "
            "or written in a way this check cannot see, or the length no longer "
            "matches the array. One quoted name per line reconciles them."
        )
    # A name written twice would make the set smaller than the length without
    # either check above noticing, and on the deterministic side that is a
    # partition that silently loses a member.
    if len(set(entries)) != len(entries):
        duplicates = sorted({entry for entry in entries if entries.count(entry) > 1})
        sys.exit(f"{MAPPING}: {name} lists {duplicates} more than once.")
    return set(entries)


def catalog_types(path: pathlib.Path) -> set[str]:
    found: set[str] = set()
    document = yaml.safe_load(path.read_text(encoding="utf-8"))
    # Both catalogs use these two top-level keys today; a third section
    # would be silently invisible here, on both sides of the comparison.
    for section in ("identifiers", "entities"):
        for entry in document.get(section) or ():
            found.add(entry["entity_type"])
    return found


def report(
    gateway: set[str], detector: set[str], gateway_name: str, catalog: pathlib.Path
) -> bool:
    """Print one side's disagreement. True when there was one."""
    missing = sorted(detector - gateway)
    extra = sorted(gateway - detector)
    if missing:
        print(f"declared in {catalog.name}, absent from {gateway_name}: {missing}")
    if extra:
        print(f"declared in {gateway_name}, absent from {catalog.name}: {extra}")
    return bool(missing or extra)


def main() -> None:
    source = MAPPING.read_text(encoding="utf-8")
    vocabulary = rust_array(source, "ENTITY_TYPES")
    deterministic = rust_array(source, "DETERMINISTIC_TYPES")

    # Checked before the catalogs, because a deterministic name that is not in
    # the vocabulary at all makes the complement below meaningless — it would
    # be subtracting a name that was never there.
    stray = sorted(deterministic - vocabulary)
    if stray:
        sys.exit(
            f"{MAPPING}: DETERMINISTIC_TYPES lists {stray}, which ENTITY_TYPES does "
            "not. A type the gateway refuses a number for and then masks as "
            "REDACTED everywhere else is neither of the two things it could be. "
            "Reconcile the two arrays and re-run `make check-entity-types`."
        )

    drifted = report(
        deterministic, catalog_types(IDENTIFIERS), "DETERMINISTIC_TYPES", IDENTIFIERS
    )
    drifted |= report(
        vocabulary - deterministic,
        catalog_types(NER),
        "ENTITY_TYPES minus DETERMINISTIC_TYPES",
        NER,
    )
    if drifted:
        sys.exit(
            "entity types have drifted; the gateway would mask the difference as "
            "REDACTED, and a name on the wrong side of the split changes whether "
            "a number carrying it is refused. Reconcile ENTITY_TYPES and "
            f"DETERMINISTIC_TYPES in {MAPPING} with the catalogs above and re-run "
            "`make check-entity-types`."
        )
    print(
        f"entity types agree: {len(vocabulary)} declared on both sides, "
        f"{len(deterministic)} of them deterministic"
    )


if __name__ == "__main__":
    main()
