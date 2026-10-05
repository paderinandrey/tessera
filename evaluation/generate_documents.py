"""Synthetic corpus of joined JSON documents, in the shape production sends (#103).

`corpus/public.jsonl` is sentences, and the joined-path tests group four of them
into a document. Measured against the one real payload this repository holds —
`gateway/src/testdata/claude_code_tools.json`, pinned in
`mapping::a_real_joined_call_is_many_short_leaves_rather_than_a_few_long_ones` —
that group has the wrong granularity: production joins **many short leaves**
(79 of them across ten schemas, median 37 characters, 42 no longer than 40),
while the group joins four of about ninety. Its joined *length* is ordinary;
its leaf count is at the bottom of what production sends.

**Two populations, and this file is honest about which it measures.** A tool
*definition* is a `Shape::Schema` document that the gateway joins and detects on
every request carrying tools, and it is what the figures above are taken from —
a production shape, not a proxy for one. A tool *argument* is a
`Shape::Instance` document, a different population, and no captured one exists
here. What the same file offers instead is a proxy: each schema declares 2 to 15
top-level properties, 47 in all. It is not a bound in either direction — a
property holding an array or object yields as many leaves as it holds, and
`Artifact.capabilities` is an open object, so nothing caps an argument; a boolean
yields none, and a property declaring no `type` can do either. For this payload
those are three properties that can yield any number of leaves and six that can
yield none, two of them untyped and in both, which makes the proxy a reasonable
one and nothing more. Both counts drive the documents below.

What stays unmeasured is an argument's real leaf count and every argument
leaf's **length**. A value is the caller's data and nothing here samples it, so
the lengths come from the definition population and are an assumption rather
than a measurement. Raised twice by review on #105: once against an earlier
version that took the definition counts and called itself the argument shape,
and once against the property counts being called a ceiling.

This generator does not touch `public.jsonl`: the published metrics are tied to
that file's digest, and a corpus regenerated for a reason unrelated to them has
moved a published number here before.

Run from the repository root:
    uv run --project detector --group eval python evaluation/generate_documents.py
"""

import json
import random
import sys
from pathlib import Path
from typing import Any

from faker import Faker
from generate import (
    ARTICLE_9_SLOTS,
    DE_TEMPLATES,
    FR_TEMPLATES,
    MIXED_TEMPLATES,
    TYPES,
    _apostrophe_or,
    render,
)

SEED = 20261005
OUTPUT = Path(__file__).parent / "corpus" / "documents.jsonl"

# Leaves per document, taken from the real payload rather than chosen.
#
# The leaves each tool *definition* presents to one detect call — a
# `Shape::Schema` walk of its `input_schema`, which is what the gateway joins
# today.
SCHEMA_LEAF_COUNTS = [8, 2, 4, 5, 2, 3, 20, 29, 2, 4]
# The properties each schema *declares*, used as a proxy for an argument's leaf
# count. Not a bound: `Shape::Instance` recurses into nested values, an open
# object has no limit, and a boolean yields no leaf. The gateway test pins which
# properties make the proxy wrong in each direction.
ARGUMENT_FIELD_COUNTS = [4, 2, 4, 5, 2, 2, 8, 15, 2, 3]
# Interleaved so the corpus covers both, document by document, rather than
# averaging them into a shape neither population has. Four — what the sentence
# corpus groups — appears here as an ordinary member rather than as the whole
# corpus.
LEAF_COUNTS = [
    count
    for pair in zip(SCHEMA_LEAF_COUNTS, ARGUMENT_FIELD_COUNTS, strict=True)
    for count in pair
]

# How many documents. Three full passes over `LEAF_COUNTS`, so each of the ten
# tools contributes its definition count and its argument count the same number
# of times — an uneven tail would weight one population over the other for no
# reason. It also lands the annotation count near the sentence corpus's 196,
# which is what makes an inventory from one comparable in size to an inventory
# from the other.
DOCUMENTS = 3 * len(LEAF_COUNTS)

# Filler leaves: what a tool call carries that holds no personal data. The
# bands exist because the measured distribution has both ends — a one-character
# leaf and a 276-character one — and a filler pool of uniform length would hit
# the median while matching the shape nowhere.
TINY = ["string", "object", "array", "auto", "high", "low", "json", "utf-8", "1", "true", "none"]
SHORT = [
    "/srv/app/config/settings.yaml",
    "req-8f21c0",
    "2026-10-05T09:14:22Z",
    "--dry-run --verbose",
    "application/json",
    "branch: release/2026.10",
    "retry after 30s",
    "sha256:4f1ac2be",
    "artifacts/2026-10/report-final.pdf",
    "timeout: 30s, retries: 3, backoff: 2x",
    "de-DE, fr-FR;q=0.8, en;q=0.5",
    "state=awaiting-review, owner=unassigned",
    "GET /v1/cases/8f21c0/attachments",
]
MEDIUM = [
    "Der Auftrag wurde angelegt und wartet auf die Freigabe durch den Fachbereich.",
    "Le rapport sera transmis au service juridique avant la fin de la semaine.",
    "Die Felder unten beschreiben den Zustand des Vorgangs zum Zeitpunkt des Abrufs.",
    "Toute modification de ce champ déclenche une nouvelle validation du dossier.",
]
WIDE = [
    "Die Antwort enthält nur die Felder, die der Aufrufer angefordert hat; alles "
    "Übrige bleibt unverändert in der Akte.",
    "Le curseur renvoyé par cet appel continue la pagination ; son absence signifie "
    "que la dernière page a été atteinte.",
    "Accepts a path relative to the working directory. An absolute path outside it is "
    "refused rather than resolved, and the call fails.",
]
LONG = [
    "Return the records matching the filter. The call is paginated and the cursor in "
    "the response continues it; an absent cursor means the last page was reached. "
    "Fields the caller did not ask for are omitted rather than returned empty, so an "
    "absent field says nothing about the record.",
    "Beschreibt den Vorgang, den dieser Aufruf anlegt. Der Text wird unverändert in "
    "die Akte übernommen und ist später nur noch über einen Korrekturvorgang zu "
    "ändern, weshalb die fachliche Prüfung vor und nicht nach dem Aufruf stattfindet.",
]

# Which share of a document's leaves carry something annotated. The rest is
# filler — a payload where every leaf holds personal data would make the joined
# path look harder than it is, and the measurement this corpus exists for is
# about what surrounding text does to a score.
ANNOTATED_SHARE = 0.45
# Of the annotated leaves, how many are a value standing alone rather than a
# sentence with a value inside it. A tool call has both: `"customer": "Lenoir"`
# and a `note` field holding prose.
BARE_SHARE = 0.7

_TEMPLATES = FR_TEMPLATES + DE_TEMPLATES + MIXED_TEMPLATES


def _bare_leaf(
    rng: random.Random, fakers: dict[str, Faker], apostrophes: random.Random
) -> dict[str, Any]:
    """One annotated value, alone in its leaf — the `"iban": "CH93…"` shape."""
    lang = rng.choice(["fr", "de"])
    faker = fakers[lang]
    kind = rng.choice([*TYPES, "email", "person", "org", "city", *ARTICLE_9_SLOTS])
    if kind in TYPES:
        entity_type, generator = TYPES[kind]
        value = generator(rng)
    elif kind == "email":
        entity_type, value = "EMAIL", faker.email()
    elif kind == "person":
        entity_type = "PERSON"
        value = _apostrophe_or(faker.last_name(), lang, apostrophes)
    elif kind == "org":
        entity_type = "ORG"
        value = f"{faker.last_name()} {faker.company_suffix()}"
    elif kind == "city":
        entity_type, value = "LOCATION", faker.city()
    else:
        entity_type, by_language = ARTICLE_9_SLOTS[kind]
        value = rng.choice(by_language[lang])
    return {
        "text": value,
        "entities": [{"entity_type": entity_type, "start": 0, "end": len(value)}],
    }


def _prose_leaf(
    rng: random.Random, fakers: dict[str, Faker], apostrophes: random.Random
) -> dict[str, Any]:
    """A sentence with values inside it — the `note` or `description` field."""
    template = rng.choice(_TEMPLATES)
    lang = "mixed" if template in MIXED_TEMPLATES else ("fr" if template in FR_TEMPLATES else "de")
    rendered = render(template, fakers, lang, rng, apostrophes)
    return {"text": rendered["text"], "entities": rendered["entities"]}


def _filler_leaf(rng: random.Random) -> dict[str, Any]:
    pool = rng.choices([TINY, SHORT, MEDIUM, WIDE, LONG], weights=[18, 30, 24, 18, 10])[0]
    return {"text": rng.choice(pool), "entities": []}


def main() -> None:
    rng = random.Random(SEED)
    # Its own stream, for the reason `generate.py` gives: a pool draw that
    # shares `rng` shifts every value after it, and the corpus then moves for a
    # reason that has nothing to do with the change being made.
    apostrophes = random.Random(SEED ^ 0x0027)
    fakers = {"fr": Faker("fr_FR"), "de": Faker("de_DE")}
    for faker in fakers.values():
        faker.seed_instance(SEED)

    documents: list[dict[str, Any]] = []
    for index in range(DOCUMENTS):
        count = LEAF_COUNTS[index % len(LEAF_COUNTS)]
        leaves: list[dict[str, Any]] = []
        for _ in range(count):
            if rng.random() < ANNOTATED_SHARE:
                if rng.random() < BARE_SHARE:
                    leaves.append(_bare_leaf(rng, fakers, apostrophes))
                else:
                    leaves.append(_prose_leaf(rng, fakers, apostrophes))
            else:
                leaves.append(_filler_leaf(rng))
        documents.append({"id": f"doc-{index:04d}", "leaves": leaves})

    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    with OUTPUT.open("w", encoding="utf-8") as handle:
        for document in documents:
            handle.write(json.dumps(document, ensure_ascii=False, sort_keys=True) + "\n")
    every_leaf = [leaf for document in documents for leaf in document["leaves"]]
    annotated = sum(len(leaf["entities"]) for leaf in every_leaf)
    print(
        f"wrote {len(documents)} documents, {len(every_leaf)} leaves, "
        f"{annotated} annotations to {OUTPUT}",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
