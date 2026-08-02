"""Synthetic public corpus generator (REQ-37, the reproducible half).

Templates per language (FR, DE, plus a code-switching section per REQ-5) are filled
with checksum-valid synthetic identifiers. Identifiers are generated, never sampled
from real data; a fixed seed makes the output byte-identical between runs so the
committed corpus can be diffed.

Run from the repository root:  uv run --project detector --group eval python evaluation/generate.py
"""

import json
import random
import re
import sys
from pathlib import Path

from faker import Faker
from schwifty import IBAN
from stdnum import luhn
from stdnum.ch import ssn as ch_ssn
from stdnum.de import idnr as de_idnr
from stdnum.fr import nif as fr_nif
from stdnum.fr import nir as fr_nir

SEED = 20260801
OUTPUT = Path(__file__).parent / "corpus" / "public.jsonl"

FR_TEMPLATES = [
    "Bonjour, le virement pour {person} partira de l'IBAN {iban} avant vendredi.",
    "Le client {person} (NIR {nir}) a demandé la clôture de son dossier.",
    "Numéro fiscal {nif} — merci de vérifier l'avis d'imposition de {person}.",
    "Contact : {email} pour toute question sur le paiement par carte {card}.",
    "Madame {person} habite à {city} ; son numéro AVS est le {avs}.",
    "La société {org} confirme le virement de {person} vers l'IBAN {iban}.",
]

DE_TEMPLATES = [
    "Sehr geehrter Herr {person}, Ihre Steuer-ID {idnr} wurde erfasst.",
    "Die Zahlung von {person} erfolgt auf das Konto {iban} bei der {org} in {city}.",
    "Bitte senden Sie die Unterlagen an {email}; die Karte {card} wurde gesperrt.",
    "Der Mandant {person} (AVS-Nummer {avs}) wohnt in {city}.",
    "Steuernummer {stnr} des Mandanten {person} liegt der {org} vor.",
    "Die {org} in {city} hat die Rechnung von {person} beglichen.",
]

MIXED_TEMPLATES = [
    "Le paiement erfolgt bis Freitag: IBAN {iban}, Ansprechpartner {person} ({email}).",
    "Kunde {person} demande le remboursement — carte {card}, NIR {nir}.",
    "Die Rechnung pour {person} référence la Steuer-ID {idnr} et l'IBAN {iban}.",
    "Der Kunde {person} de la société {org} demande un remboursement — IBAN {iban}.",
]

CLEAN_TEMPLATES = [
    "La réunion de jeudi validera le budget du prochain trimestre.",
    "Die Lieferung verzögert sich wegen des Feiertags um zwei Werktage.",
    "Merci de confirmer la réception de ce message avant la fin de la semaine.",
]


def _nir(rng: random.Random) -> str:
    while True:
        sex = rng.choice("12")
        base = (
            f"{sex}{rng.randint(50, 99)}{rng.randint(1, 12):02d}"
            f"{rng.randint(1, 95):02d}{rng.randint(1, 990):03d}{rng.randint(1, 999):03d}"
        )
        key = 97 - int(base) % 97
        candidate = f"{base}{key:02d}"
        if fr_nir.is_valid(candidate):
            g = candidate
            return f"{g[0]} {g[1:3]} {g[3:5]} {g[5:7]} {g[7:10]} {g[10:13]} {g[13:15]}"


def _nif(rng: random.Random) -> str:
    while True:
        candidate = f"{rng.choice('0123')}{rng.randint(0, 10**12 - 1):012d}"
        if fr_nif.is_valid(candidate):
            g = candidate
            return f"{g[:2]} {g[2:4]} {g[4:7]} {g[7:10]} {g[10:13]}"


def _avs(rng: random.Random) -> str:
    while True:
        candidate = f"756{rng.randint(0, 10**10 - 1):010d}"
        if ch_ssn.is_valid(candidate):
            g = candidate
            return f"{g[:3]}.{g[3:7]}.{g[7:11]}.{g[11:13]}"


def _idnr(rng: random.Random) -> str:
    while True:
        candidate = f"{rng.randint(10**10, 10**11 - 1)}"
        if de_idnr.is_valid(candidate):
            g = candidate
            return f"{g[:2]} {g[2:5]} {g[5:8]} {g[8:11]}"


def _stnr(rng: random.Random) -> str:
    return f"{rng.randint(100, 999)}/{rng.randint(100, 999)}/{rng.randint(10000, 99999)}"


def _card(rng: random.Random) -> str:
    while True:
        base = "4" + "".join(str(rng.randint(0, 9)) for _ in range(14))
        check = next(str(d) for d in range(10) if luhn.is_valid(base + str(d)))
        g = base + check
        return f"{g[:4]} {g[4:8]} {g[8:12]} {g[12:16]}"


def _iban(rng: random.Random) -> str:
    country, bank = rng.choice([("DE", "37040044"), ("CH", "00762"), ("FR", "3000600001")])
    account = f"{rng.randint(0, 10**9 - 1):010d}"
    iban = IBAN.generate(country, bank_code=bank, account_code=account)
    return iban.formatted


TYPES = {
    "iban": ("IBAN", _iban),
    "nir": ("FR_NIR", _nir),
    "nif": ("FR_NIF", _nif),
    "avs": ("CH_AVS", _avs),
    "idnr": ("DE_STEUER_ID", _idnr),
    "stnr": ("DE_STEUERNUMMER", _stnr),
    "card": ("CREDIT_CARD", _card),
}


def render(
    template: str, fakers: dict[str, Faker], lang: str, rng: random.Random
) -> dict[str, object]:
    faker = fakers[lang if lang != "mixed" else rng.choice(["fr", "de"])]
    text, entities = "", []
    for token in _tokenize(template):
        if token.startswith("{"):
            name = token.strip("{}")
            if name in TYPES:
                entity_type, generator = TYPES[name]
                value = generator(rng)
                entities.append(
                    {"entity_type": entity_type, "start": len(text), "end": len(text) + len(value)}
                )
                text += value
            elif name == "email":
                value = faker.email()
                entities.append(
                    {"entity_type": "EMAIL", "start": len(text), "end": len(text) + len(value)}
                )
                text += value
            elif name in ("person", "city", "org"):
                value = {
                    "person": faker.last_name,
                    "city": faker.city,
                    # faker.company() often returns a bare surname, which no
                    # annotator could tell from a PERSON; a suffix makes the
                    # gold label decidable.
                    "org": lambda: f"{faker.last_name()} {faker.company_suffix()}",
                }[name]()
                entity_type = {"person": "PERSON", "city": "LOCATION", "org": "ORG"}[name]
                entities.append(
                    {"entity_type": entity_type, "start": len(text), "end": len(text) + len(value)}
                )
                text += value
        else:
            text += token
    return {"lang": lang, "text": text, "entities": entities}


def _tokenize(template: str) -> list[str]:
    return [t for t in re.split(r"(\{\w+\})", template) if t]


def main() -> None:
    rng = random.Random(SEED)
    fakers = {"fr": Faker("fr_FR"), "de": Faker("de_DE")}
    for f in fakers.values():
        f.seed_instance(SEED)
    documents = []
    pools = [("fr", FR_TEMPLATES, 30), ("de", DE_TEMPLATES, 30), ("mixed", MIXED_TEMPLATES, 20)]
    for lang, templates, count in pools:
        for i in range(count):
            doc = render(templates[i % len(templates)], fakers, lang, rng)
            doc["id"] = f"{lang}-{i:04d}"
            documents.append(doc)
    for i, template in enumerate(CLEAN_TEMPLATES * 5):
        documents.append(
            {"id": f"clean-{i:04d}", "lang": "clean", "text": template, "entities": []}
        )
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    with OUTPUT.open("w", encoding="utf-8") as handle:
        for doc in documents:
            handle.write(json.dumps(doc, ensure_ascii=False, sort_keys=True) + "\n")
    print(f"wrote {len(documents)} documents to {OUTPUT}", file=sys.stderr)


if __name__ == "__main__":
    main()
