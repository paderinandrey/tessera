.PHONY: corpus evaluate test lint

corpus:
	uv run --project detector --group eval python evaluation/generate.py

evaluate:
	uv run --project detector python evaluation/evaluate.py

test:
	cd detector && uv run pytest

lint:
	cd detector && uv run ruff check . ../evaluation && uv run mypy src
