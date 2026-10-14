"""Synthetic corpus of JSON documents whose leaves the gateway joins (#105).

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

# Both lists are checked per tool against gateway/src/testdata/claude_code_tools.json
# by `mapping::a_real_joined_call_is_many_short_leaves_rather_than_a_few_long_ones`.
SCHEMA_LEAF_COUNTS = [8, 2, 4, 5, 2, 3, 20, 29, 2, 4]
ARGUMENT_FIELD_COUNTS = [4, 2, 4, 5, 2, 2, 8, 15, 2, 3]
LEAF_COUNTS = [
    count
    for pair in zip(SCHEMA_LEAF_COUNTS, ARGUMENT_FIELD_COUNTS, strict=True)
    for count in pair
]

DOCUMENTS = 3 * len(LEAF_COUNTS)

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

ANNOTATED_SHARE = 0.45
BARE_SHARE = 0.7

_TEMPLATES = FR_TEMPLATES + DE_TEMPLATES + MIXED_TEMPLATES


def _bare_leaf(
    rng: random.Random, fakers: dict[str, Faker], apostrophes: random.Random
) -> dict[str, Any]:
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
    template = rng.choice(_TEMPLATES)
    lang = "mixed" if template in MIXED_TEMPLATES else ("fr" if template in FR_TEMPLATES else "de")
    rendered = render(template, fakers, lang, rng, apostrophes)
    return {"text": rendered["text"], "entities": rendered["entities"]}


def _filler_leaf(rng: random.Random) -> dict[str, Any]:
    pool = rng.choices([TINY, SHORT, MEDIUM, WIDE, LONG], weights=[18, 30, 24, 18, 10])[0]
    return {"text": rng.choice(pool), "entities": []}


def main() -> None:
    rng = random.Random(SEED)
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
