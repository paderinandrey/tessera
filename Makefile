.PHONY: corpus evaluate bench openapi test lint model

corpus:
	uv run --project detector --group eval python evaluation/generate.py

evaluate:
	uv run --project detector python evaluation/evaluate.py

bench:
	uv run --project detector --group ner python evaluation/benchmark.py

openapi:
	uv run --project detector --group serve python evaluation/export_openapi.py

test:
	cd detector && uv run --group serve pytest

lint:
	cd detector && uv run ruff check . ../evaluation && uv run mypy src

model:
	uv run --project detector --group ner python -c "\
	from huggingface_hub import snapshot_download; \
	from tessera_detector.models import HF_REPO_ID, HF_REVISION, model_cache_dir; \
	p = model_cache_dir(); p.parent.mkdir(parents=True, exist_ok=True); \
	snapshot_download(HF_REPO_ID, revision=HF_REVISION, local_dir=str(p), \
	ignore_patterns=['onnx/model_*.onnx']); \
	print('weights in', p)"
