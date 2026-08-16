"""Fail when the gateway's entity-type vocabulary and the detector's catalogs
disagree.

The gateway holds its own copy on purpose — asking the detector which types it
emits would be worthless against a detector that lies. The copy is only safe
while something notices it drifting, and that is this script.
"""

import pathlib
import re
import sys

import yaml

ROOT = pathlib.Path(__file__).resolve().parent.parent
MAPPING = ROOT / "gateway" / "src" / "mapping.rs"
CATALOGS = (
    ROOT / "detector" / "src" / "tessera_detector" / "catalog" / "identifiers.yaml",
    ROOT / "detector" / "src" / "tessera_detector" / "catalog" / "ner.yaml",
)

DECLARATION = re.compile(
    r"pub const ENTITY_TYPES: \[&str; (\d+)\] = \[(.*?)\];", re.DOTALL
)
# One array element, alone on its line: `    "PERSON",`. The shape of the entry
# is matched rather than the shape of a name so that `ARTICLE9` — a name the
# placeholder grammar would not admit — is seen and reported rather than
# silently skipped as unparsable.
ENTRY = re.compile(r'^\s*"([^"]*)",\s*$', re.MULTILINE)


def char_literal(source: str, index: int) -> int:
    """Length of the character literal at `index`, or 0 if there is none."""
    width = 4 if source[index + 1 : index + 2] == "\\" else 3
    return width if source[index + width - 1 : index + width] == "'" else 0


def uncommented(source: str) -> str:
    """`source` with every comment replaced by blanks, as the compiler sees it.

    Three versions of this check in a row were defeated by a comment spelling
    nobody had listed: a quoted name in a `//` comment, then the same in a
    one-line `/* */`, then an entry inside a `/* */` spanning lines. Each fix
    answered its example. This one answers the question instead — no comment
    text of either form, at any nesting depth, reaches the entry matcher.

    A scanner and not a `re.sub`, because Rust's block comments nest: against
    `/* outer /* inner */ "PERSON", */` a non-greedy `/\\*.*?\\*/` stops at the
    first `*/` and hands the entry to the matcher as if it were code. String
    literals are tracked for the mirror-image reason — `"/*"` is a legal entry
    and must not open a comment — and so are character literals, since `'"'`
    elsewhere in the file would otherwise open a string that never closes.
    Newlines survive so the entry matcher still sees one entry per line.
    """
    out: list[str] = []
    depth = 0
    in_string = False
    index = 0
    while index < len(source):
        char, pair = source[index], source[index : index + 2]
        if depth:
            if pair in ("/*", "*/"):
                depth += 1 if pair == "/*" else -1
                out.append("  ")
                index += 2
                continue
            out.append("\n" if char == "\n" else " ")
            index += 1
        elif in_string:
            # A backslash escape is copied whole, so that `"\""` does not end
            # the literal on its escaped quote.
            width = 2 if char == "\\" else 1
            in_string = char != '"'
            out.append(source[index : index + width])
            index += width
        elif pair == "/*":
            depth = 1
            out.append("  ")
            index += 2
        elif pair == "//":
            end = source.find("\n", index)
            end = len(source) if end == -1 else end
            out.append(" " * (end - index))
            index = end
        elif char == "'" and (width := char_literal(source, index)):
            # `'"'`, copied whole. A lifetime is left alone: `&'static str` has
            # no closing quote where a character literal would have one.
            out.append(source[index : index + width])
            index += width
        else:
            in_string = char == '"'
            out.append(char)
            index += 1
    return "".join(out)


def gateway_types() -> set[str]:
    # The whole file, not just the declaration: a commented-out copy of an older
    # ENTITY_TYPES above the real one would otherwise be the first thing the
    # declaration pattern found.
    match = DECLARATION.search(uncommented(MAPPING.read_text(encoding="utf-8")))
    if match is None:
        sys.exit(f"{MAPPING}: no ENTITY_TYPES declaration found")
    declared, entries = int(match.group(1)), ENTRY.findall(match.group(2))
    # The array's own length, cross-checked against the entries read out of it.
    # This is the half that does not depend on knowing every way to hide a name:
    # whatever the trick, an entry the compiler does not see is one the length
    # does not count, and a length that disagrees with the array does not build.
    # So a name visible to only one of the two sides shows up here as a miscount
    # — `#[cfg(...)]` on an entry does, and no comment rule would have caught it
    # — and a name both sides see is compared below. The anchor is the written
    # length: were `ENTITY_TYPES` ever to become a slice with no length in its
    # type, the pattern above stops matching and this script exits saying so
    # rather than quietly losing half its guarantee.
    if len(entries) != declared:
        sys.exit(
            f"{MAPPING}: ENTITY_TYPES declares {declared} entries and {len(entries)} "
            "were read from it. One of them is written in a way this check cannot "
            "see — reformat the array to one quoted name per line."
        )
    return set(entries)


def detector_types() -> set[str]:
    found: set[str] = set()
    for path in CATALOGS:
        document = yaml.safe_load(path.read_text(encoding="utf-8"))
        # Both catalogs use these two top-level keys today; a third section
        # would be silently invisible here, on both sides of the comparison.
        for section in ("identifiers", "entities"):
            for entry in document.get(section) or ():
                found.add(entry["entity_type"])
    return found


def main() -> None:
    gateway, detector = gateway_types(), detector_types()
    if gateway == detector:
        print(f"entity types agree: {len(gateway)} declared on both sides")
        return

    missing = sorted(detector - gateway)
    extra = sorted(gateway - detector)
    if missing:
        print(f"declared by the detector, absent from the gateway: {missing}")
    if extra:
        print(f"declared by the gateway, absent from the detector: {extra}")
    sys.exit(
        "entity types have drifted; the gateway would mask the difference as "
        f"REDACTED. Reconcile ENTITY_TYPES in {MAPPING} with the catalogs above "
        "and re-run `make check-entity-types`."
    )


if __name__ == "__main__":
    main()
