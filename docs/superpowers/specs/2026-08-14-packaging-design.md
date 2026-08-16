# Packaging: two containers, and a prototype that runs

## The problem

The README's design principles promise "two containers (Rust gateway + Python
detector)", and the architecture section describes a self-hosted deployment. The
repository contains no Dockerfile and no compose file. Everything works — the
gateway masks, restores, streams, holds sessions and journals — but running it
means starting two processes by hand, in two languages, with a 2 GB model
download in between.

So the gap between this repository and a prototype anyone can see is not a
feature. It is packaging, and it is the only thing in that position.

One friction we added ourselves: `audit_path` became a required key with no
default (PR #18), and `gateway/tessera.example.toml` points it at
`/var/log/tessera/audit.jsonl`. A developer following the README's own quickstart
now fails at startup, because that directory does not exist and is not writable
without root. Correct behaviour meeting a bad default — [#21](https://github.com/paderinandrey/tessera/issues/21).

## What this slice is for

**Demo first, without dead ends.** The measure is that someone with Docker and
no other setup sees the gateway masking real-looking text within minutes. But
every decision is taken so that a self-hoster can use the same artifacts later:
persistent volumes rather than container-local state, an unprivileged user,
pinned versions, and no development-only shortcut baked into the default path.

Publishing to a registry, image signing, Kubernetes manifests and a
production-hardening pass are explicitly not in this slice.

## The decisions

1. **Weights live on a named volume, downloaded by one explicit command**, not
   baked into the image and not fetched silently at startup.

2. **The gateway gains `GET /health`**, so orchestration has something better
   than an open TCP port to ask.

3. **The container's configuration is its own file**, not the host example and
   not environment variables layered over the parser.

4. **A stand-in provider ships under a compose overlay**, so the demo needs no
   API key and no tokens.

5. **The TLS stack does not change.** Packaging does not get to decide how the
   gateway validates a provider's certificate.

### Why the weights are not in the image

They are 2.0 GB, from a HuggingFace repository at a revision pinned in
`models.py`. Baking them in produces a ~3 GB image that rebuilds on every code
change and pushes 3 GB to move a one-line fix.

Fetching them automatically on first start is worse for the product's own
reasons: a gateway sold into a closed perimeter should not reach the internet on
its own, and a first `docker compose up` that silently blocks for minutes on a
2 GB download is indistinguishable from one that has hung.

So: `docker compose run --rm weights` fills the volume once, visibly, and a
failure there reads as a failed download rather than a failed service. Until it
is run the gateway works on the deterministic layer alone — checksum-validated
identifiers, which is the layer that scores 1.000 — and `/health` reports
`"ner": false` with the reason. That degradation already exists and is already
honest; packaging does not need to hide it.

The same volume mount covers the case that matters for anyone who has used the
CLI: they already have those 2 GB in `~/.cache/tessera/models`, and can bind-mount
that directory instead of downloading again.

### Why the weights mount where the code already looks

`find_model()` reads `TESSERA_NER_MODEL` first, and falls back to
`~/.cache/tessera/models/<name>@<revision>` — a path that includes the pinned
revision, deliberately, so that a cache filled before a revision bump cannot keep
serving stale weights.

Setting `TESSERA_NER_MODEL` in compose would restate that path in a second place
and make the revision bump a two-file change, with the compose half easy to
forget. Instead the volume mounts at the container user's
`~/.cache/tessera/models`, and `find_model()` resolves it with no configuration
at all.

### Why the detector image is large, and what it is not

The `ner` dependency group pulls `gliner`, which requires `torch` and
`transformers` even though inference runs through ONNX Runtime. From PyPI, the
Linux torch wheel drags the whole CUDA stack behind it and the image lands at
5.34 GB. Taken from PyTorch's CPU index instead (below) it is 1.32 GB: torch
637 MB installed, `transformers` 109 MB, `onnxruntime` 51 MB, a 1.2 GB virtual
environment before any weights.

The remainder is a real cost and it is not this slice's to fix. Removing the
torch dependency means either vendoring the tokenizer GLiNER uses or driving ONNX
Runtime directly — a change to the detector's NER layer with its own quality
risk, measured against the existing gates. Recorded here so the number is not
mistaken for something packaging did.

### Why the build trusts a second package index

`[[tool.uv.index]] pytorch-cpu` in `detector/pyproject.toml` takes torch from
PyTorch's own CPU wheel index rather than PyPI. What that bought: nineteen
packages left the lock — eighteen CUDA and NVIDIA runtimes plus `triton` — and
the image fell from 5.34 GB to 1.32 GB. All of it shipped in every detector
image, none of it was loaded by anything, and all of it was attack surface for a
GPU this product never opens.

What it cost is a second trust root. Every Linux install now fetches its torch
wheel from infrastructure PyTorch operates instead of from PyPI, and that is a
supply-chain decision, not a size optimisation. Two hosts are involved and they
are not the same one: the declared index is `download.pytorch.org/whl/cpu`, but
it lists its wheels with absolute links to `download-r2.pytorch.org`, so every
torch URL in `uv.lock` is on the latter. A build (`uv sync --locked`) therefore
fetches its wheel from `download-r2.pytorch.org`, and regenerating the lock
(`uv lock`) reads the listing from `download.pytorch.org`. Whether a locked build
also reaches the declared index — for a listing it does not need, since it has
the URL and the hash — has not been tested here, so an operator running an egress
allowlist should permit both hosts rather than infer one from this paragraph.

Three things bound it. `explicit = true`, so the index is consulted for torch and
for nothing else — no other package can be resolved from it, deliberately or by a
name that happens to exist there. A `sys_platform == 'linux'` marker, so macOS
stays on PyPI, where torch's wheels are CPU-only already and the file is
byte-identical; a second host to trust buys nothing there. And a per-wheel sha256
in the lock, which is what reduces the claim from "we trust this infrastructure"
to "we trusted it once, at lock time, and pinned the result" — a compromise of
that host after the lock was written produces a hash mismatch and a failed build,
not a silent substitution.

### Why the TLS stack stays as it is

`reqwest` is built with default features, so on Linux it links OpenSSL. A
distroless runtime would require switching to `rustls`, which would produce a
smaller image with no system libraries — and would change which root store the
gateway validates provider certificates against.

That is a security decision. It may well be the right one, and it deserves its
own slice with its own reasoning; it does not get made as a side effect of
wanting a smaller image. The runtime is `debian-slim` with `ca-certificates`,
`libssl3` and `curl`.

## Topology

Four services. Two run by default.

| service | profile | published | purpose |
|---|---|---|---|
| `detector` | — | no | detection HTTP contract, internal only |
| `gateway` | — | `8080` | the proxy |
| `weights` | `setup` | no | one-shot, fills the models volume |
| `mock-provider` | demo overlay | no | stands in for OpenAI |

**Only the gateway is published.** `/detect` takes arbitrary text and
authenticates nobody; exposing it on the host would hand out a way to run text
through the model outside the gateway, and therefore outside the journal. It is
reachable on the compose network and nowhere else.

`weights` sits under a profile so that `up` ignores it. It shares the models
volume and runs the same download the `model` Makefile target runs.

## Volumes

Two named volumes.

**`models`** → the detector's `~/.cache/tessera/models`, read-only. Written only
by the `weights` service.

**`audit`** → `/var/lib/tessera` in the gateway, holding both `audit.jsonl` and
`audit.jsonl.salt`.

**Both audit files must be on the same persistent volume, and this is not a
preference.** A journal that has lines in it with no salt beside it refuses to
start — the rule this project added deliberately, because silently minting a new
salt renumbers every tenant mid-journal. Keeping the salt in the image layer, or
on a different volume than the journal, produces a gateway that will not start
after the first container recreation. The compose file is where that rule is
either honoured or discovered the hard way.

Ownership is handled in the Dockerfile: the mountpoint is created with the
service user as owner, and Docker copies that ownership when it initialises an
empty named volume from the image. Without it an unprivileged process meets a
root-owned volume on first run.

## Images

Both multi-stage, both running as an unprivileged user.

**Gateway.** Build in `rust:<pinned>-slim` with `cargo build --release`; the
runtime stage carries the binary, `ca-certificates`, `libssl3` and `curl`. The
Dockerfile pins a Rust version — the repository pins none today (`Cargo.toml`
carries only `edition = "2021"` and CI takes `stable`), so a build today and a
build in six months are different builds. `curl` is there for the healthcheck and
because an image people will debug should have one.

**Detector.** Build with `uv sync --locked --group serve --group ner`; the
runtime stage carries the virtualenv and the sources. The `ner` group is not
optional: without ONNX Runtime the weights volume cannot be used at all, and the
image would silently be deterministic-only forever.

## Configuration

`deploy/tessera.container.toml`, mounted read-only. Three values differ from the
host example, each of them load-bearing:

- `detector_url = "http://detector:8000"` — the service name, not loopback.
- `audit_path = "/var/lib/tessera/audit.jsonl"` — inside the volume.
- `bind = "0.0.0.0:8080"` — a container that binds loopback is unreachable from
  outside itself.

The host example keeps `127.0.0.1` and gets a default `audit_path` a developer
can actually write, which closes [#21](https://github.com/paderinandrey/tessera/issues/21).

Environment-variable overrides are deliberately not added. The config parser
rejects unknown keys so that a typo in a security control fails loudly; layering
an env-var path over it is a change to that control, wanted by nobody yet.

## Health and ordering

The detector already serves `GET /health`, reporting whether the NER layer is
loaded and why it is not.

The gateway gains one. **It reports that the process is alive and its journal is
open, and it does not probe the detector.** An unauthenticated endpoint that
reaches the detector on request would be a way to drive the detector without a
credential — and the gateway's own liveness does not depend on the detector's,
since a detector failure is a per-request refusal by design, not a startup
condition.

The gateway waits for `detector: condition: service_healthy`, so the first
request does not land on a detector still starting. The detector's healthcheck
needs a generous `start_period`: with weights present it loads 2 GB of model
before it answers.

Healthchecks use `curl` in the gateway image and a `python -c` urllib call in the
detector image, since neither runtime carries a shell HTTP client by default.

## The demo overlay

`docker compose -f docker-compose.yml -f deploy/docker-compose.demo.yml up` adds
`mock-provider` and replaces the gateway's mounted config with one whose
`openai_base` and `anthropic_base` point at it.

An overlay rather than a profile, because a profile cannot override another
service's configuration — and leaving a fictitious provider address in the
production config, switched by a variable, is exactly the kind of default nobody
should be one typo away from shipping.

The stand-in records the request it received and answers using the placeholders
it was given, so a single run shows both directions: what left the perimeter, and
what came back restored.

## Testing

Not "the image builds". A smoke test that exercises the guarantee.

`make compose-smoke` brings the stack up with the demo overlay and **without**
weights — the deterministic layer alone, so the test needs no 2 GB download and
still has checksum-validated identifiers to work with. It sends one request
carrying a valid IBAN and an email address, then asserts:

- the stand-in provider received placeholders, and none of the submitted values;
- the client received the real values back, restored;
- the journal holds a `masked` line and an `outcome` line, and no submitted value,
  placeholder name or raw session id anywhere in the file.

Then the regression that this slice's own constraint demands:

- after `docker compose up -d --force-recreate --no-deps gateway`, the gateway
  starts and appends to the same journal — proving the salt and the journal
  survived together on one volume, which is the failure a first user would
  otherwise find for us. Recreated rather than restarted: `restart` reuses the
  container and therefore its writable layer, so a journal written outside any
  volume survives it and the assertion passes while proving nothing.

And one negative check, because the topology claims it:

- the detector's port is not reachable from the host.

**In CI this runs on pushes to `main`, not on every pull request.** Building a
1.3 GB image on every PR costs more than the test catches at that frequency; the
existing three jobs already gate the code itself.

## Out of scope

Publishing images to a registry, tags and release versioning, image signing and
SBOMs, Kubernetes manifests or a Helm chart, resource limits and restart
policies tuned for production, log shipping, and the torch-free detector image.
Per-credential quotas and the detection-quality work remain where they were.
