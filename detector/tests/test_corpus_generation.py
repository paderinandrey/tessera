import hashlib
import os
from pathlib import Path

import pytest

# `make test` syncs the serve group, which has no `faker`, so these skip there.
# The CI step that runs them syncs the eval group and sets this variable, the
# same job `--require-ner` does for the NER gates: without it the step would go
# green on a skip, which is the one outcome that looks identical to a pass.
if os.environ.get("TESSERA_REQUIRE_EVAL_DEPS"):
    import generate
    import generate_documents
else:
    pytest.importorskip("faker", reason="generation needs the eval group")
    import generate
    import generate_documents

CORPUS = Path(__file__).resolve().parents[2] / "evaluation" / "corpus"
COMMITTED = CORPUS / "public.jsonl"
COMMITTED_DOCUMENTS = CORPUS / "documents.jsonl"


def _generate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, runs: int) -> list[str]:
    out = tmp_path / "public.jsonl"
    monkeypatch.setattr(generate, "OUTPUT", out)
    digests = []
    for _ in range(runs):
        generate.main()
        digests.append(hashlib.sha256(out.read_bytes()).hexdigest())
    return digests


def test_generation_is_stable_within_one_process(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # CI regenerates the corpus once per run and diffs it, which proves
    # freshness but starts a fresh interpreter every time. A generator holding
    # state across calls is invisible to that: `main()` reseeds `rng` and every
    # `Faker`, so anything else it draws from has to be reseeded with them.
    first, second = _generate(tmp_path, monkeypatch, runs=2)
    assert first == second


def test_generation_reproduces_the_committed_corpus(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    (digest,) = _generate(tmp_path, monkeypatch, runs=1)
    assert digest == hashlib.sha256(COMMITTED.read_bytes()).hexdigest()


def _generate_documents(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, runs: int
) -> list[str]:
    out = tmp_path / "documents.jsonl"
    monkeypatch.setattr(generate_documents, "OUTPUT", out)
    digests = []
    for _ in range(runs):
        generate_documents.main()
        digests.append(hashlib.sha256(out.read_bytes()).hexdigest())
    return digests


def test_document_generation_is_stable_within_one_process(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The same property as above and it is not inherited: this generator draws
    # from `generate`'s `render`, which draws from the `Faker` instances *this*
    # `main()` seeds, and from an apostrophe stream of its own.
    first, second = _generate_documents(tmp_path, monkeypatch, runs=2)
    assert first == second


def test_document_generation_reproduces_the_committed_corpus(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    (digest,) = _generate_documents(tmp_path, monkeypatch, runs=1)
    assert digest == hashlib.sha256(COMMITTED_DOCUMENTS.read_bytes()).hexdigest()


def test_the_document_generator_does_not_touch_the_published_corpus(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """`public.jsonl` is what the published metrics are tied to by digest.

    The two generators share `render`, `TYPES` and the apostrophe pool, so a
    change made for the document corpus reaches the sentence one — and a corpus
    regenerated for an unrelated reason has moved a published number here
    before. Asserted rather than remembered.
    """
    before = hashlib.sha256(COMMITTED.read_bytes()).hexdigest()
    _generate_documents(tmp_path, monkeypatch, runs=1)
    assert hashlib.sha256(COMMITTED.read_bytes()).hexdigest() == before
