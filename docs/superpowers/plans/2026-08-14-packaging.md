# Packaging implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `docker compose up` brings up a working Tessera gateway, and a demo overlay shows it masking real-looking text without anyone needing a provider API key.

**Architecture:** Two multi-stage images — a Rust gateway and a Python detector — plus a compose file with two named volumes. The 2 GB of NER weights are not in either image: a one-shot `weights` service under a profile fills a volume that mounts where `find_model()` already looks. A demo overlay adds a stand-in provider and swaps the gateway's config for one pointing at it.

**Tech Stack:** Docker (29.x) and Compose v5, `rust:1.97-slim-trixie` and `debian:trixie-slim`, `ghcr.io/astral-sh/uv` and `python:3.14-slim-trixie`, axum (for the new health route).

**Spec:** `docs/superpowers/specs/2026-08-14-packaging-design.md`

## Global Constraints

- **The journal and its salt share one persistent volume.** A journal with lines and no salt beside it refuses to start. `audit_path` in the container config points inside the `audit` volume, and both files land there.
- **Only the gateway publishes a port.** `/detect` authenticates nobody; publishing it would be a way to run text through the model outside the gateway and therefore outside the journal.
- **Both images run as an unprivileged user**, and the mountpoint for a named volume is created in the image owned by that user — that is what makes Docker initialise an empty volume with the right ownership.
- **The TLS stack does not change.** `reqwest` keeps its default features (OpenSSL); the runtime image carries `ca-certificates` and `libssl3`. Do not switch to `rustls` or distroless.
- **The weights path is written down once**, in `models.py`. The volume mounts onto `~/.cache/tessera/models` so `find_model()` resolves it with no env var and no second copy of the pinned revision.
- **`bind = "0.0.0.0:8080"` in the container config.** A container binding loopback is unreachable from outside itself.
- Rust is pinned at `1.97` in the Dockerfile; the repository pins none today.
- `cd gateway && cargo test` and `cargo fmt --check && cargo clippy --all-targets -- -D warnings` must pass after any Rust change.
- Commit messages follow the repository's style: `feat(gateway):`, `feat(deploy):`, `fix(gateway):`, `test(deploy):`, `docs:`.
- If `git commit` fails with a 1Password or signing-agent error, do not work around it — report it and stop; the controller commits on your behalf.

## File structure

| path | responsibility |
|---|---|
| `gateway/src/proxy.rs` | gains `GET /health` and its tests |
| `gateway/Dockerfile` | multi-stage Rust build, unprivileged runtime, volume mountpoint |
| `gateway/.dockerignore` | keeps `target/` out of the build context |
| `detector/Dockerfile` | multi-stage uv build, unprivileged runtime, weights mountpoint |
| `detector/.dockerignore` | keeps `.venv/`, caches and corpora out of the context |
| `docker-compose.yml` | detector, gateway, `weights` (profile `setup`), two volumes |
| `deploy/tessera.container.toml` | the gateway's configuration inside the stack |
| `deploy/docker-compose.demo.yml` | overlay: stand-in provider + demo config |
| `deploy/tessera.demo.toml` | as the container config, but pointing at the stand-in |
| `deploy/mock_provider.py` | the stand-in provider |
| `deploy/smoke.py` | the end-to-end smoke test |
| `gateway/tessera.example.toml` | `audit_path` default a developer can write (#21) |
| `Makefile` | `compose-smoke` |
| `.github/workflows/ci.yml` | a `compose` job, `main` only |
| `README.md` | the quickstart this slice makes true |

---

### Task 1: `GET /health` on the gateway

**Files:**
- Modify: `gateway/src/proxy.rs` (the `router` function and the test module)

**Interfaces:**
- Consumes: nothing.
- Produces: `GET /health` → `200` with body `{"status":"ok"}`. Nothing later depends on the body's shape beyond the status code, which is what the healthcheck reads.

This is Rust, not Docker, and it comes first because the gateway image's healthcheck calls it.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `gateway/src/proxy.rs`:

```rust
    #[tokio::test]
    async fn health_answers_without_a_credential() {
        // An orchestrator has no API key and must still be able to ask.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/chat/completions", json!({})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("routed");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn health_does_not_drive_the_detector() {
        // An unauthenticated endpoint that reached the detector on request
        // would be a way to run detection without a credential — and a
        // detector outage is a per-request refusal by design, not a reason to
        // call this gateway unhealthy.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/chat/completions", json!({})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());

        router(state)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("routed");

        assert!(
            detector
                .received_requests()
                .await
                .expect("recorded")
                .is_empty(),
            "health must not call the detector"
        );
    }

    #[tokio::test]
    async fn health_writes_no_audit_record() {
        // A liveness probe runs every few seconds forever. Journaling it would
        // bury the evidence under lines about nothing.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/chat/completions", json!({})).await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());

        router(state)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("routed");

        let journal = std::fs::read_to_string(&path).expect("readable");
        assert!(journal.is_empty(), "health wrote to the journal: {journal}");
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cd gateway && cargo test proxy::tests::health`
Expected: FAIL — the router has no `/health` route, so axum answers `404` and the first assertion fails.

- [ ] **Step 3: Add the route**

In `gateway/src/proxy.rs`, extend the axum routing import:

```rust
use axum::routing::{get, post};
```

Add the handler immediately above `pub fn router`:

```rust
/// Liveness for an orchestrator: this process is up, and it is up *with* a
/// journal, since `main` opens the journal before it binds and a failure
/// there stops the process rather than starting one that proves nothing.
///
/// It deliberately reports nothing about the detector. This endpoint takes no
/// credential, so probing the detector from here would be a way to drive
/// detection without one; and a detector outage refuses individual requests
/// by design rather than making this gateway unhealthy.
async fn health() -> Response {
    (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response()
}
```

Add the route in `router`:

```rust
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/chat/completions", post(openai))
        .route("/v1/messages", post(anthropic))
        .with_state(state)
}
```

- [ ] **Step 4: Run the tests**

Run: `cd gateway && cargo test`
Expected: PASS — the three new tests plus the existing suite.

- [ ] **Step 5: Prove the third test discriminates**

Temporarily make `health` write a record — construct a `Record` and drop it — and confirm `health_writes_no_audit_record` fails. Restore. Report the observed failure.

- [ ] **Step 6: Commit**

```bash
cd gateway && cargo fmt && cargo clippy --all-targets -- -D warnings
cd .. && git add gateway/src/proxy.rs
git commit -m "feat(gateway): answer GET /health

An orchestrator needs something better than an open TCP port to ask, and a
port accepts connections before a service is ready. The answer covers this
process only: it is up, and it is up with a journal, since main opens the
journal before it binds.

It does not probe the detector. This endpoint takes no credential, so
reaching the detector from here would be a way to drive detection without
one, and a detector outage is a per-request refusal by design rather than
a reason to report this gateway unhealthy. It writes no audit record
either: a probe every few seconds would bury the evidence."
```

---

### Task 2: the gateway image

**Files:**
- Create: `gateway/Dockerfile`, `gateway/.dockerignore`

**Interfaces:**
- Consumes: `GET /health` from Task 1.
- Produces: an image whose entrypoint is `tessera-gateway` and whose default argument is `/etc/tessera/tessera.toml`; a `/var/lib/tessera` directory owned by uid 10001; `EXPOSE 8080`.

- [ ] **Step 1: Write the ignore file**

Create `gateway/.dockerignore`:

```
target
tessera.example.toml
```

`target/` is several gigabytes of build output and would be copied into the build context on every build.

- [ ] **Step 2: Write the Dockerfile**

Create `gateway/Dockerfile`:

```dockerfile
# syntax=docker/dockerfile:1

# Pinned rather than `stable`: the repository pins no toolchain, so a build
# today and a build in six months are otherwise different builds.
FROM rust:1.97-slim-trixie AS build
# `openssl-sys` links system OpenSSL and finds it through pkg-config; the slim
# image carries neither, so without this the build fails at link time rather
# than at any point that names OpenSSL. Build stage only — these never reach
# the runtime image.
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/tessera-gateway /usr/local/bin/tessera-gateway

FROM debian:trixie-slim
# `reqwest` is built with default features, so the binary links OpenSSL.
# Switching to rustls would shrink this image and would also change which
# root store validates a provider's certificate — a security decision, and
# not one packaging gets to make. `curl` serves the healthcheck and the
# person debugging this container at 3am.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl libssl3 \
 && rm -rf /var/lib/apt/lists/*
# The journal and its salt live here and must share one persistent volume: a
# journal with lines and no salt beside it refuses to start. Creating the
# directory owned by the service user is what makes Docker initialise an
# empty named volume with that ownership — otherwise an unprivileged process
# meets a root-owned mount on first run.
RUN useradd --system --uid 10001 --create-home tessera \
 && mkdir -p /var/lib/tessera \
 && chown tessera:tessera /var/lib/tessera
COPY --from=build /usr/local/bin/tessera-gateway /usr/local/bin/tessera-gateway
USER tessera
EXPOSE 8080
ENTRYPOINT ["tessera-gateway"]
CMD ["/etc/tessera/tessera.toml"]
```

- [ ] **Step 3: Build it**

Run: `docker build -t tessera-gateway:dev gateway`
Expected: a successful build. Note the image size from `docker image ls tessera-gateway:dev`.

- [ ] **Step 4: Verify the image refuses to start without a journal**

This is the fail-closed guarantee, seen from outside:

```bash
docker run --rm tessera-gateway:dev /nonexistent/tessera.toml
```

Expected: a non-zero exit and an error naming the missing configuration — not a served port. Record the output.

- [ ] **Step 5: Verify it serves health with a real config and volume**

```bash
docker volume create tessera-smoke-audit
printf 'bind = "0.0.0.0:8080"\ndetector_url = "http://127.0.0.1:8000"\naudit_path = "/var/lib/tessera/audit.jsonl"\n' > /tmp/t.toml
docker run --rm -d --name tessera-smoke \
  -v tessera-smoke-audit:/var/lib/tessera \
  -v /tmp/t.toml:/etc/tessera/tessera.toml:ro \
  -p 18080:8080 tessera-gateway:dev
sleep 2
curl -fsS http://127.0.0.1:18080/health
docker rm -f tessera-smoke && docker volume rm tessera-smoke-audit
```

Expected: `{"status":"ok"}`. This also proves the volume is writable by uid 10001 — the gateway cannot start otherwise, because it opens the journal before it binds.

- [ ] **Step 6: Commit**

```bash
git add gateway/Dockerfile gateway/.dockerignore
git commit -m "feat(deploy): the gateway image

Multi-stage: the Rust build image is a gigabyte and a half and the binary
is a few megabytes. Rust is pinned here because the repository pins it
nowhere.

The runtime keeps OpenSSL rather than moving to rustls and distroless: the
smaller image would also change which root store validates a provider's
certificate, which is a security decision and not one packaging makes.

/var/lib/tessera is created owned by the service user, which is what makes
Docker initialise an empty named volume with that ownership. The journal
and its salt both live there and must stay together — a journal with lines
and no salt beside it refuses to start."
```

---

### Task 3: the detector image

**Files:**
- Create: `detector/Dockerfile`, `detector/.dockerignore`

**Interfaces:**
- Consumes: nothing.
- Produces: an image whose entrypoint is `tessera` with default arguments `serve --host 0.0.0.0`, serving on 8000, with `~/.cache/tessera/models` present and owned by the service user.

- [ ] **Step 1: Write the ignore file**

Create `detector/.dockerignore`:

```
.venv
.pytest_cache
.ruff_cache
.mypy_cache
**/__pycache__
tests
```

- [ ] **Step 2: Write the Dockerfile**

Create `detector/Dockerfile`:

```dockerfile
# syntax=docker/dockerfile:1

FROM ghcr.io/astral-sh/uv:python3.14-trixie-slim AS build
WORKDIR /src
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
COPY --from=build --chown=tessera:tessera /src/.venv /app/.venv
ENV PATH="/app/.venv/bin:$PATH"
USER tessera
# find_model() looks exactly here. The models volume mounts onto it, so the
# pinned revision in models.py stays the only place that path is written
# down — an env var would be a second copy to forget on the next bump.
RUN mkdir -p /home/tessera/.cache/tessera/models
EXPOSE 8000
ENTRYPOINT ["tessera"]
CMD ["serve", "--host", "0.0.0.0"]
```

- [ ] **Step 3: Build it**

Run: `docker build -t tessera-detector:dev detector`
Expected: a successful build. Record the image size; the spec predicts roughly 1 GB, and a large divergence is worth reporting.

- [ ] **Step 4: Verify the catalogs shipped**

The failure this catches is silent: a wheel without the YAML catalogs starts fine and detects nothing.

```bash
docker run --rm tessera-detector:dev scan /dev/stdin <<'EOF'
Meine IBAN lautet CH9300762011623852957.
EOF
```

Expected: a report naming `IBAN`. If it reports nothing, `identifiers.yaml` did not make it into the installation — stop and report rather than working around it.

- [ ] **Step 5: Verify it serves health without weights**

```bash
docker run --rm -d --name tessera-detector-smoke -p 18000:8000 tessera-detector:dev
sleep 5
curl -fsS http://127.0.0.1:18000/health
docker rm -f tessera-detector-smoke
```

Expected: `{"status":"ok","ner":false,"ner_off_reason":"no weights"}` or equivalent — `ner: false` is correct here and is the honest degradation the design relies on.

- [ ] **Step 6: Commit**

```bash
git add detector/Dockerfile detector/.dockerignore
git commit -m "feat(deploy): the detector image

Dependencies are installed before the sources are copied, so editing code
does not re-download torch — which gliner requires even though inference
runs through ONNX Runtime, and which is most of this image's size.

--no-editable so the runtime stage carries a real installation and no
sources. The catalog YAML travels with it: deterministic.py and ner.py
read them through importlib.resources, and a wheel without them would be a
detector that starts cleanly and finds nothing.

The weights mountpoint is the path find_model() already looks at, so the
pinned revision in models.py stays the only place it is written down."
```

---

### Task 4: the compose stack

**Files:**
- Create: `docker-compose.yml`, `deploy/tessera.container.toml`
- Modify: `gateway/tessera.example.toml`

**Interfaces:**
- Consumes: both images from Tasks 2 and 3.
- Produces: services `detector`, `gateway`, `weights`; volumes `models`, `audit`. The gateway is reachable on host port `8080`.

- [ ] **Step 1: Write the container configuration**

Create `deploy/tessera.container.toml`:

```toml
# The gateway's configuration inside the compose stack. Three values differ
# from gateway/tessera.example.toml, and each has to:
#
#   - detector_url names the compose service, not loopback;
#   - audit_path points inside the audit volume, which is also where the
#     salt file lands — they must share one volume, because a journal with
#     lines and no salt beside it refuses to start;
#   - bind is 0.0.0.0, because a container that binds loopback is
#     unreachable from outside itself.

bind = "0.0.0.0:8080"
detector_url = "http://detector:8000"
detector_timeout_secs = 60

openai_base = "https://api.openai.com"
anthropic_base = "https://api.anthropic.com"

session_idle_secs = 1800
max_sessions = 1000
max_session_values = 1000

audit_path = "/var/lib/tessera/audit.jsonl"
```

- [ ] **Step 2: Fix the host example's audit path**

In `gateway/tessera.example.toml`, replace the `audit_path` line and its comment with:

```toml
# Where the audit journal is appended, one JSON object per line. Required: the
# gateway does not start without it. Two records per request — one written and
# fsynced before the provider is called, one when the request ends — and neither
# ever carries submitted text. A salt file is kept beside it as
# <audit_path>.salt; both hold evidence, they must stay together, and a journal
# whose salt has gone missing refuses to start rather than renumbering every
# tenant beneath you.
#
# This default is relative on purpose, so that following the quickstart writes
# where you ran it. A deployment wants an absolute path on storage you back up.
audit_path = "tessera-audit.jsonl"
```

Add `tessera-audit.jsonl*` to `.gitignore` at the repository root, so a developer's quickstart does not offer to commit their own journal.

- [ ] **Step 3: Write the compose file**

Create `docker-compose.yml`:

```yaml
# Two services run by default. Only the gateway publishes a port: /detect
# takes arbitrary text and authenticates nobody, so exposing it would hand
# out a way to run text through the model outside the gateway — and
# therefore outside the journal.

services:
  detector:
    build: ./detector
    image: tessera-detector:dev
    volumes:
      # Mounted where find_model() already looks. Read-only: only the
      # `weights` service writes here.
      - models:/home/tessera/.cache/tessera/models:ro
    healthcheck:
      # No shell HTTP client in a slim Python image, and none needed.
      test: ["CMD", "python", "-c", "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8000/health').read()"]
      interval: 5s
      timeout: 5s
      retries: 12
      # With weights present the model loads before /health answers, and 2 GB
      # of ONNX takes a while.
      start_period: 180s
    restart: unless-stopped

  gateway:
    build: ./gateway
    image: tessera-gateway:dev
    depends_on:
      detector:
        condition: service_healthy
    ports:
      - "8080:8080"
    volumes:
      - audit:/var/lib/tessera
      - ./deploy/tessera.container.toml:/etc/tessera/tessera.toml:ro
    healthcheck:
      test: ["CMD", "curl", "-fsS", "http://127.0.0.1:8080/health"]
      interval: 5s
      timeout: 3s
      retries: 5
      start_period: 10s
    restart: unless-stopped

  # One-shot, and under a profile so `up` ignores it:
  #   docker compose run --rm weights
  # A gateway sold into a closed perimeter should not reach the internet on
  # its own, and a first `up` that silently blocks on 2 GB is
  # indistinguishable from one that has hung.
  weights:
    profiles: ["setup"]
    build: ./detector
    image: tessera-detector:dev
    volumes:
      - models:/home/tessera/.cache/tessera/models
    entrypoint: ["python", "-c"]
    command:
      - |
        from huggingface_hub import snapshot_download
        from tessera_detector.models import HF_REPO_ID, HF_REVISION, model_cache_dir
        p = model_cache_dir()
        p.parent.mkdir(parents=True, exist_ok=True)
        snapshot_download(HF_REPO_ID, revision=HF_REVISION, local_dir=str(p),
                          ignore_patterns=["onnx/model_*.onnx"])
        print("weights in", p)

volumes:
  models:
  # The journal and its salt both live here. They must not be separated.
  audit:
```

- [ ] **Step 4: Bring the stack up without weights**

```bash
docker compose up -d --build
docker compose ps
curl -fsS http://127.0.0.1:8080/health
```

Expected: both services healthy, `{"status":"ok"}` from the gateway.

- [ ] **Step 5: Verify the detector is not reachable from the host**

The topology claims this, so check it rather than assume it:

```bash
curl -sS --max-time 3 http://127.0.0.1:8000/health && echo "REACHABLE — this is a defect" || echo "not reachable, as intended"
```

Expected: not reachable.

- [ ] **Step 6: Verify the journal survives a restart**

This is the constraint the audit slice created, seen from outside:

```bash
curl -fsS -X POST http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' -H 'authorization: Bearer sk-smoke' \
  -d '{"messages":[{"role":"user","content":"IBAN CH9300762011623852957"}]}' > /dev/null || true
docker compose restart gateway
sleep 5
docker compose exec gateway sh -c 'wc -l < /var/lib/tessera/audit.jsonl'
curl -fsS http://127.0.0.1:8080/health
```

Expected: the gateway comes back healthy and the journal still has its lines. The upstream call fails without a real API key — that is fine, the refusal is journaled and that is what this step checks.

- [ ] **Step 7: Tear down and commit**

```bash
docker compose down -v
git add docker-compose.yml deploy/tessera.container.toml gateway/tessera.example.toml .gitignore
git commit -m "feat(deploy): the compose stack

Two services by default, and only the gateway publishes a port: /detect
authenticates nobody, so exposing it would be a way to run text through the
model outside the gateway and therefore outside the journal.

The weights service sits under a profile rather than running on up. A
gateway sold into a closed perimeter should not reach the internet on its
own, and a first up that silently blocks on 2 GB is indistinguishable from
one that has hung.

The audit volume holds the journal and its salt together, which is not a
preference: separated, the gateway refuses to start after the first
container recreation. The host example's audit_path becomes relative so
the README's own quickstart works where you ran it (#21)."
```

---

### Task 5: the demo overlay

**Files:**
- Create: `deploy/mock_provider.py`, `deploy/docker-compose.demo.yml`, `deploy/tessera.demo.toml`

**Interfaces:**
- Consumes: the stack from Task 4.
- Produces: `docker compose -f docker-compose.yml -f deploy/docker-compose.demo.yml up` serves a gateway whose provider is the stand-in. The stand-in writes what it received to `/received/received.json` inside its container and answers with the placeholders it was given.

- [ ] **Step 1: Write the stand-in provider**

Create `deploy/mock_provider.py`:

```python
"""A stand-in for OpenAI and Anthropic that records what actually reached it.

The demo's whole point is this file's contents: if the gateway works, nothing
here is a real name, IBAN or diagnosis. It answers using the placeholders it
was handed, so restoration on the way back is visible in the same run.
"""

import json
import re
from http.server import BaseHTTPRequestHandler, HTTPServer

RECEIVED = "/received/received.json"
PLACEHOLDER = re.compile(r"\[[A-Z_]{1,40}_\d+\]")


class Handler(BaseHTTPRequestHandler):
    def do_POST(self) -> None:
        raw = self.rfile.read(int(self.headers["Content-Length"]))
        body = json.loads(raw)
        with open(RECEIVED, "w") as f:
            json.dump({"path": self.path, "body": body}, f, indent=2)

        seen = PLACEHOLDER.findall(json.dumps(body, ensure_ascii=False))
        listed = ", ".join(dict.fromkeys(seen)) or "nothing identifiable"
        reply = f"Eingang bestätigt. Betroffen sind: {listed}."

        if self.path.startswith("/v1/messages"):
            payload = {
                "id": "msg_demo",
                "type": "message",
                "role": "assistant",
                "model": "claude-demo",
                "content": [{"type": "text", "text": reply}],
                "stop_reason": "end_turn",
            }
        else:
            payload = {
                "id": "chatcmpl-demo",
                "object": "chat.completion",
                "model": "gpt-demo",
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": reply},
                        "finish_reason": "stop",
                    }
                ],
            }

        out = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)

    def do_GET(self) -> None:
        self.send_response(200)
        self.send_header("content-length", "2")
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, *args: object) -> None:
        pass


HTTPServer(("0.0.0.0", 9099), Handler).serve_forever()
```

- [ ] **Step 2: Write the demo configuration**

Create `deploy/tessera.demo.toml` — identical to the container config except for the two provider bases:

```toml
# The container configuration with both providers pointed at the stand-in, so
# the demo needs no API key and spends no tokens. This file is mounted only by
# the demo overlay: a fictitious provider address must never be one variable
# away from a production start.

bind = "0.0.0.0:8080"
detector_url = "http://detector:8000"
detector_timeout_secs = 60

openai_base = "http://mock-provider:9099"
anthropic_base = "http://mock-provider:9099"

session_idle_secs = 1800
max_sessions = 1000
max_session_values = 1000

audit_path = "/var/lib/tessera/audit.jsonl"
```

- [ ] **Step 3: Write the overlay**

Create `deploy/docker-compose.demo.yml`:

```yaml
# Adds a stand-in provider and points the gateway at it:
#
#   docker compose -f docker-compose.yml -f deploy/docker-compose.demo.yml up
#
# An overlay rather than a profile, because a profile cannot override another
# service's configuration — and a fictitious provider address switched by a
# variable is exactly the default nobody should be one typo away from
# shipping.

services:
  mock-provider:
    image: python:3.14-slim-trixie
    volumes:
      - ./deploy/mock_provider.py:/mock_provider.py:ro
      - received:/received
    command: ["python", "/mock_provider.py"]
    healthcheck:
      test: ["CMD", "python", "-c", "import urllib.request; urllib.request.urlopen('http://127.0.0.1:9099/').read()"]
      interval: 5s
      timeout: 3s
      retries: 5

  gateway:
    depends_on:
      detector:
        condition: service_healthy
      mock-provider:
        condition: service_healthy
    volumes:
      - audit:/var/lib/tessera
      - ./deploy/tessera.demo.toml:/etc/tessera/tessera.toml:ro

volumes:
  received:
```

**Verify the mount actually wins before moving on.** Compose merges a service's
`volumes` list across files, and two entries with the same container path is a
conflict whose resolution you should observe rather than assume. Check with:

```bash
docker compose -f docker-compose.yml -f deploy/docker-compose.demo.yml config \
  | grep -A 12 'gateway:' | grep -A 6 volumes
```

Expected: one mount at `/etc/tessera/tessera.toml`, sourced from
`tessera.demo.toml`. If both appear, or the base file's wins, move the config
mount out of `docker-compose.yml` into a `deploy/docker-compose.prod.yml`
overlay so each run names exactly one config, and update the README's commands
to match. **Report which shape you landed on and what `config` printed** — this
is the one place in this plan where compose's merge semantics decide the file
layout rather than the design does.

- [ ] **Step 4: Bring up the demo and see it work**

```bash
docker compose -f docker-compose.yml -f deploy/docker-compose.demo.yml up -d --build
sleep 5
curl -fsS -X POST http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' -H 'authorization: Bearer sk-demo' \
  -d '{"messages":[{"role":"user","content":"Meine IBAN lautet CH9300762011623852957, erreichbar unter weber@example.ch."}]}'
docker compose -f docker-compose.yml -f deploy/docker-compose.demo.yml exec mock-provider cat /received/received.json
```

Expected: the client's response carries the real IBAN and email restored; `received.json` carries `[IBAN_1]` and `[EMAIL_2]` and neither real value.

- [ ] **Step 5: Tear down and commit**

```bash
docker compose -f docker-compose.yml -f deploy/docker-compose.demo.yml down -v
git add deploy/mock_provider.py deploy/docker-compose.demo.yml deploy/tessera.demo.toml
git commit -m "feat(deploy): a demo overlay with a stand-in provider

Someone evaluating a privacy gateway should not need an API key and a
token budget to see whether it masks anything. The stand-in records what
reached it and answers with the placeholders it was handed, so one run
shows both directions: what left the perimeter and what came back.

An overlay rather than a profile: a profile cannot override another
service's configuration, and a fictitious provider address switched by a
variable is the kind of default nobody should be one typo away from
shipping."
```

---

### Task 6: the smoke test

**Files:**
- Create: `deploy/smoke.py`
- Modify: `Makefile`, `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: the demo overlay from Task 5.
- Produces: `make compose-smoke`, exit code 0 on success and non-zero with a named failure otherwise.

- [ ] **Step 1: Write the smoke test**

Create `deploy/smoke.py`:

```python
"""End-to-end check of the packaged stack, without weights and without a key.

Not "the images build" — that is what a build already proves. This asserts the
guarantee the product is sold on: that what reaches the provider carries no
personal data, that the client gets its data back, and that the journal
records the request without becoming a copy of it.

Run through `make compose-smoke`, which brings the stack up first.
"""

import json
import subprocess
import sys
import urllib.request

COMPOSE = [
    "docker", "compose",
    "-f", "docker-compose.yml",
    "-f", "deploy/docker-compose.demo.yml",
]

IBAN = "CH9300762011623852957"
EMAIL = "weber@example.ch"
TEXT = f"Meine IBAN lautet {IBAN}, erreichbar bin ich unter {EMAIL}."


def compose(*args: str) -> str:
    result = subprocess.run(
        [*COMPOSE, *args], capture_output=True, text=True, check=True
    )
    return result.stdout


def fail(message: str) -> None:
    print(f"smoke: {message}", file=sys.stderr)
    sys.exit(1)


def main() -> None:
    request = urllib.request.Request(
        "http://127.0.0.1:8080/v1/chat/completions",
        data=json.dumps({"messages": [{"role": "user", "content": TEXT}]}).encode(),
        headers={"content-type": "application/json", "authorization": "Bearer sk-smoke"},
    )
    with urllib.request.urlopen(request, timeout=120) as response:
        answer = json.load(response)
    restored = answer["choices"][0]["message"]["content"]

    # 1. The provider saw placeholders and no values.
    received = compose("exec", "-T", "mock-provider", "cat", "/received/received.json")
    if IBAN in received or EMAIL in received:
        fail("a submitted value reached the provider")
    if "[IBAN_" not in received:
        fail(f"the provider did not receive a masked IBAN; it got: {received}")

    # 2. The client got its values back. The stand-in echoes the placeholders
    #    it was given, so a restored answer names the real IBAN.
    if IBAN not in restored:
        fail(f"the client did not get the value restored; it got: {restored}")

    # 3. The journal recorded the request and quotes nothing.
    journal = compose("exec", "-T", "gateway", "cat", "/var/lib/tessera/audit.jsonl")
    lines = [json.loads(line) for line in journal.splitlines() if line.strip()]
    events = [line["event"] for line in lines]
    if events[-2:] != ["masked", "outcome"]:
        fail(f"expected a masked line then an outcome line, got: {events}")
    for forbidden in (IBAN, EMAIL, "IBAN_1", "sk-smoke"):
        if forbidden in journal:
            fail(f"the journal carries {forbidden!r}")
    if lines[-1]["result"] != "completed":
        fail(f"the request did not complete: {lines[-1]}")

    # 4. The detector is not reachable from the host. The topology claims it.
    try:
        urllib.request.urlopen("http://127.0.0.1:8000/health", timeout=3)
    except Exception:
        pass
    else:
        fail("the detector is published to the host")

    # 5. The journal and its salt survive a restart together. Separated, the
    #    gateway refuses to start — the failure a first user would find for us.
    before = len(lines)
    compose("restart", "gateway")
    for _ in range(30):
        try:
            urllib.request.urlopen("http://127.0.0.1:8080/health", timeout=2).read()
            break
        except Exception:
            import time

            time.sleep(2)
    else:
        fail("the gateway did not come back after a restart")
    journal = compose("exec", "-T", "gateway", "cat", "/var/lib/tessera/audit.jsonl")
    after = len([line for line in journal.splitlines() if line.strip()])
    if after < before:
        fail(f"the journal lost lines across a restart: {before} then {after}")

    print(f"smoke: ok ({after} journal lines, nothing quoted)")


if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Add the Makefile target**

Add to `Makefile`, and add `compose-smoke` to the `.PHONY` line:

```make
compose-smoke:
	docker compose -f docker-compose.yml -f deploy/docker-compose.demo.yml up -d --build
	@echo "waiting for the gateway"
	@for i in $$(seq 1 60); do \
	  curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 && break; \
	  sleep 2; \
	done
	python3 deploy/smoke.py; status=$$?; \
	docker compose -f docker-compose.yml -f deploy/docker-compose.demo.yml down -v; \
	exit $$status
```

- [ ] **Step 3: Run it, and watch it pass**

Run: `make compose-smoke`
Expected: `smoke: ok (...)` and exit 0.

- [ ] **Step 4: Prove each assertion discriminates**

The smoke test is worthless if it passes whatever the stack does. Break the stack four ways, one at a time, and confirm the matching assertion fails:

1. Point `deploy/tessera.demo.toml`'s `detector_url` at a port nothing listens on → the request is refused, and the test fails on the restored-value or journal-result check rather than passing.
2. Publish the detector in `docker-compose.yml` (`ports: ["8000:8000"]`) → assertion 4 fails.
3. Mount the audit volume at a different path in the gateway so the salt and journal separate across a restart → assertion 5 fails.
4. Make the stand-in echo the request body verbatim instead of the placeholder list → assertion 1 or 3 fails.

Restore after each. Report the four observed failures — a smoke test whose failures nobody has seen is a guess.

- [ ] **Step 5: Add the CI job**

Add to `.github/workflows/ci.yml`, as a fourth job:

```yaml
  compose:
    # Only on main: building a ~1 GB detector image on every pull request
    # costs more than this catches at that frequency, and the three jobs
    # above already gate the code itself.
    if: github.ref == 'refs/heads/main' && github.event_name == 'push'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
      - uses: docker/setup-buildx-action@v3
      - run: make compose-smoke
```

- [ ] **Step 6: Commit**

```bash
git add deploy/smoke.py Makefile .github/workflows/ci.yml
git commit -m "test(deploy): a smoke test that asserts the guarantee

Not that the images build — a build already proves that. This sends one
request through the packaged stack and checks that the provider received
placeholders and no values, that the client got its values back, that the
journal recorded the request without quoting it, that the detector is not
published to the host, and that the journal and its salt survive a restart
together.

The last one is this slice's own constraint: separated, the gateway
refuses to start after the first container recreation, and that is a
failure a first user would otherwise find for us.

On main only. Building a gigabyte of torch on every pull request costs
more than it catches at that frequency."
```

---

### Task 7: the quickstart

**Files:**
- Modify: `README.md`

**Interfaces:**
- Consumes: everything above. Produces nothing code depends on.

- [ ] **Step 1: Write the section**

Insert into `README.md` immediately after the `## Architecture` section and before `## CLI`:

```markdown
## Running it

Two containers, and nothing else to install:

```
docker compose up -d --build              # gateway on 127.0.0.1:8080
docker compose run --rm weights           # once: 2 GB of NER weights
```

The gateway serves before the weights are there. Without them the detector runs
its deterministic layer alone — checksum-validated identifiers, the layer that
scores 1.000 — and says so at `GET /health`, so a partial install is visible
rather than silent. The download is a separate command on purpose: a gateway
that belongs inside your perimeter should not reach the internet on its own,
and a first start that blocks for minutes is indistinguishable from one that
has hung. If you have already run `make model`, mount `~/.cache/tessera/models`
into the `models` volume instead and skip the download.

Only the gateway is published. The detector answers on the compose network and
nowhere else: `POST /detect` takes arbitrary text and authenticates nobody, so
exposing it would be a way to run text through the model outside the gateway,
and therefore outside the audit journal.

The journal and its salt share the `audit` volume and must stay together — a
journal with records whose salt has gone missing refuses to start rather than
silently renumbering every tenant beneath you. Back up that volume, not just
the file.

### Seeing it work without an API key

```
docker compose -f docker-compose.yml -f deploy/docker-compose.demo.yml up -d --build
curl -X POST http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' -H 'authorization: Bearer sk-demo' \
  -d '{"messages":[{"role":"user","content":"Meine IBAN lautet CH9300762011623852957."}]}'
```

The overlay replaces the provider with a stand-in that records what reached it,
so one request shows both directions at once:

```
docker compose -f docker-compose.yml -f deploy/docker-compose.demo.yml \
  exec mock-provider cat /received/received.json
```

The answer you got back carries the real IBAN. What the provider received
carries `[IBAN_1]`. `make compose-smoke` asserts exactly that, plus that the
journal recorded the request without quoting any of it.
```

- [ ] **Step 2: Check the surrounding claims are now true**

The architecture block says "Sessions and audit are the next slices"; both have landed. Correct that line to describe what the gateway does today. Do not restate the Audit section — one clause is enough.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: a quickstart that works

Two commands to a running gateway, and one overlay to see it mask
something without an API key. Also corrects the architecture block, which
still described sessions and audit as upcoming."
```

---

## Self-review

Checked against `docs/superpowers/specs/2026-08-14-packaging-design.md`:

| Spec requirement | Task |
|---|---|
| Weights on a named volume, one explicit command | 4 (`weights` service) |
| Weights mount where `find_model()` looks, no env var | 3 (mountpoint), 4 (volume) |
| Gateway `GET /health`, no detector probe | 1 |
| Container config is its own file; three values differ | 4 |
| Host example's `audit_path` fixed (#21) | 4 |
| No env-var overrides in the config parser | — (nothing added; constraint honoured by omission) |
| Stand-in provider under an overlay, not a profile | 5 |
| TLS stack unchanged; `ca-certificates`, `libssl3`, `curl` | 2 |
| Multi-stage images, unprivileged user | 2, 3 |
| Volume mountpoint owned by the service user | 2 |
| Journal and salt on one volume | 4 (compose), 6 (asserted) |
| Only the gateway published | 4 (compose), 6 (asserted) |
| `depends_on: service_healthy`, generous `start_period` | 4 |
| `ner` group mandatory in the detector image | 3 |
| Smoke test asserts the guarantee, not the build | 6 |
| CI on `main` only | 6 |

Two things this plan adds that the spec did not name, both because leaving them
out would ship a hole:

- **Task 3 Step 4** verifies the catalog YAML survived the wheel build. The
  spec assumed it; a wheel without `identifiers.yaml` produces a detector that
  starts cleanly and finds nothing, which no other step would catch.
- **Task 6 Step 4** requires the smoke test's own assertions to be broken and
  observed failing. This branch's predecessor shipped four tests that could not
  fail; a smoke test is the easiest possible place to repeat that.

One place where the plan defers to reality rather than deciding: **Task 5
Step 3** on compose merge semantics for the gateway's config mount. The
implementer verifies which mount wins and reports the shape they landed on.
