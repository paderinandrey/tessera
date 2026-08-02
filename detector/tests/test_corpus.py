import json
from itertools import pairwise
from pathlib import Path

CORPUS = Path(__file__).resolve().parents[2] / "evaluation" / "corpus" / "public.jsonl"


def _documents() -> list[dict]:
    return [json.loads(line) for line in CORPUS.read_text(encoding="utf-8").splitlines()]


def test_corpus_annotates_the_ner_types() -> None:
    found = {e["entity_type"] for doc in _documents() for e in doc["entities"]}
    assert {"PERSON", "LOCATION", "ORG"} <= found


def test_every_annotation_matches_a_non_empty_slice() -> None:
    for doc in _documents():
        text = doc["text"]
        for entity in doc["entities"]:
            assert text[entity["start"] : entity["end"]].strip(), (
                f"empty span in {doc['id']}: {entity}"
            )


def test_annotations_do_not_overlap() -> None:
    for doc in _documents():
        spans = sorted((e["start"], e["end"]) for e in doc["entities"])
        for (_, end_a), (start_b, _) in pairwise(spans):
            assert end_a <= start_b, f"overlapping gold spans in {doc['id']}"
