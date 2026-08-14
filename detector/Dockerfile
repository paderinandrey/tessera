# syntax=docker/dockerfile:1

FROM ghcr.io/astral-sh/uv:python3.14-trixie-slim AS build
# Matches the runtime stage's venv path: uv bakes the venv's own absolute
# path into each console-script shebang, so building at /src and copying to
# /app/.venv would leave `tessera` pointing at a python3 that doesn't exist
# in the runtime image.
WORKDIR /app
ENV UV_COMPILE_BYTECODE=1 UV_LINK_MODE=copy
# Dependencies before sources, so a code change does not re-resolve or
# re-download torch.
COPY pyproject.toml uv.lock ./
RUN --mount=type=cache,target=/root/.cache/uv \
    uv sync --locked --no-install-project --group serve --group ner
COPY README.md ./
COPY src ./src
# --no-editable so the venv carries a real installation and the runtime
# stage needs no sources. The catalog YAML under src/tessera_detector must
# come with it: `deterministic.py` and `ner.py` read them through
# importlib.resources, and a wheel without them is a detector with no
# deterministic layer.
RUN --mount=type=cache,target=/root/.cache/uv \
    uv sync --locked --no-editable --group serve --group ner

FROM python:3.14-slim-trixie
RUN useradd --system --uid 10002 --create-home tessera
COPY --from=build --chown=tessera:tessera /app/.venv /app/.venv
ENV PATH="/app/.venv/bin:$PATH"
USER tessera
# find_model() looks exactly here. The models volume mounts onto it, so the
# pinned revision in models.py stays the only place that path is written
# down — an env var would be a second copy to forget on the next bump.
RUN mkdir -p /home/tessera/.cache/tessera/models
EXPOSE 8000
ENTRYPOINT ["tessera"]
CMD ["serve", "--host", "0.0.0.0"]
