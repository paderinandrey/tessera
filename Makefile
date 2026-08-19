.PHONY: corpus evaluate bench openapi test lint model gateway-test gateway-lint compose-smoke check-entity-types check-layers check-base-install

# The host port the stack publishes. Overridable because 8080 is a popular
# port; both compose and the smoke test read it from the environment.
TESSERA_PORT ?= 8080
export TESSERA_PORT

# Its own compose project, because the recipe ends in `down -v`. Without -p the
# project name comes from this directory and is `tessera` — the same project the
# documented `docker compose up -d --build` creates — so the teardown would take
# the developer's audit journal, its salt and the 2 GB weights volume with it.
# Isolated, the worst a running dev stack can cause is a port collision, which
# fails loudly.
COMPOSE_DEMO = docker compose -p tessera-smoke -f docker-compose.yml -f deploy/docker-compose.demo.yml

corpus:
	uv run --project detector --group eval python evaluation/generate.py

evaluate:
	uv run --project detector python evaluation/evaluate.py

bench:
	uv run --project detector --group ner python evaluation/benchmark.py

openapi:
	uv run --project detector --group serve python evaluation/export_openapi.py

gateway-test:
	cd gateway && cargo test

gateway-lint:
	cd gateway && cargo fmt --check && cargo clippy --all-targets -- -D warnings

test:
	cd detector && uv run --group serve pytest

lint:
	cd detector && uv run ruff check . ../evaluation && uv run mypy src

check-entity-types:
	uv run --project detector python scripts/check_entity_types.py

check-layers:
	python3 scripts/check_layers.py

# Proves detector/'s runtime imports resolve with nothing but
# [project.dependencies] installed — the exact gap that let a `packaging`
# import ship undeclared, because every environment this repo's own
# commands otherwise build includes the dev group (uv's default) or the
# serve/ner groups, any of which can drag a dependency in incidentally. A
# `pytest` run cannot substitute for this: pytest is itself a dev
# dependency, so a suite run already proves the environment it happens to
# run in, never the minimal one.
#
# Synced with UV_PROJECT_ENVIRONMENT pointed at a scratch directory rather
# than detector/.venv, so this never touches the developer's own
# environment — nothing here needs restoring, because nothing here is
# shared. `mktemp -d` for the scratch path and a trap for its removal, one
# shell for the whole recipe so the trap covers both the sync and the run:
# the same reasons compose-smoke's own recipe already uses both.
check-base-install:
	@dir=$$(mktemp -d); \
	trap 'rm -rf "$$dir"' EXIT INT TERM; \
	UV_PROJECT_ENVIRONMENT="$$dir" uv run --project detector --no-default-groups --frozen python -c "\
	from tessera_detector.pipeline import build_detector; \
	build_detector(ner=False); \
	print('base install: runtime imports resolve without dev, serve or ner')"

# Brings the demo stack up, sends one request through it, and tears the stack
# and its volumes down whether the check passed or failed — a smoke test that
# leaves a gateway and a journal behind has changed the machine it ran on.
#
# One shell for the whole recipe, because that promise is otherwise only kept
# when the check itself runs: as separate recipe lines a failed `up` aborts
# make before the teardown is ever reached, which is what happens whenever the
# published port is already taken or the detector misses its health deadline.
compose-smoke:
	@status=0; \
	trap '$(COMPOSE_DEMO) down -v; exit 130' INT TERM; \
	$(COMPOSE_DEMO) up -d --build || status=$$?; \
	if [ $$status -eq 0 ]; then \
	  echo "waiting for the gateway on port $(TESSERA_PORT)"; \
	  for i in $$(seq 1 60); do \
	    curl -fsS http://127.0.0.1:$(TESSERA_PORT)/health >/dev/null 2>&1 && break; \
	    sleep 2; \
	  done; \
	  python3 deploy/smoke.py || status=$$?; \
	fi; \
	$(COMPOSE_DEMO) down -v; \
	exit $$status

model:
	uv run --project detector --group ner python -c "\
	from huggingface_hub import snapshot_download; \
	from tessera_detector.models import HF_REPO_ID, HF_REVISION, model_cache_dir; \
	p = model_cache_dir(); p.parent.mkdir(parents=True, exist_ok=True); \
	snapshot_download(HF_REPO_ID, revision=HF_REVISION, local_dir=str(p), \
	ignore_patterns=['onnx/model_*.onnx']); \
	print('weights in', p)"
