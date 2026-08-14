"""End-to-end check of the packaged stack, without weights and without a key.

Not "the images build" — that is what a build already proves. This asserts the
guarantee the product is sold on: that what reaches the provider carries no
personal data, that the client gets its data back, and that the journal
records the request without becoming a copy of it.

Run through `make compose-smoke`, which brings the stack up first. Set
TESSERA_PORT to match if the stack was published somewhere other than 8080.
"""

import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request
from typing import NoReturn

COMPOSE = [
    "docker", "compose",
    "-f", "docker-compose.yml",
    "-f", "deploy/docker-compose.demo.yml",
]

PORT = os.environ.get("TESSERA_PORT", "8080")
GATEWAY = f"http://127.0.0.1:{PORT}"

IBAN = "CH9300762011623852957"
EMAIL = "weber@example.ch"
TEXT = f"Meine IBAN lautet {IBAN}, erreichbar bin ich unter {EMAIL}."

# The gateway numbers placeholders from one counter shared across entity types
# (gateway/src/mapping.rs), so which number the IBAN gets depends on the order
# the detector emitted the spans in. The token has to be read back from the
# run rather than predicted.
MINTED_IBAN = re.compile(r"\[IBAN_\d+\]")


def compose(*args: str) -> str:
    result = subprocess.run(
        [*COMPOSE, *args], capture_output=True, text=True, check=True
    )
    return result.stdout


def fail(message: str) -> NoReturn:
    print(f"smoke: {message}", file=sys.stderr)
    sys.exit(1)


def ask() -> str:
    """One request through the gateway, returning the restored answer.

    A gateway that cannot reach its detector answers with a status rather
    than a body, and that is a failure of the guarantee like any other — so
    it is named here instead of surfacing as a traceback.
    """
    request = urllib.request.Request(
        f"{GATEWAY}/v1/chat/completions",
        data=json.dumps({"messages": [{"role": "user", "content": TEXT}]}).encode(),
        headers={"content-type": "application/json", "authorization": "Bearer sk-smoke"},
    )
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            answer = json.load(response)
    except urllib.error.HTTPError as error:
        fail(f"the gateway refused the request: {error.code} {error.read()[:400]!r}")
    except urllib.error.URLError as error:
        fail(f"the gateway is not answering on {GATEWAY}: {error}")
    return answer["choices"][0]["message"]["content"]


def read_file(service: str, path: str) -> str:
    """Read a file out of a running container, naming the failure if absent.

    A missing file here means the stack did not do what it claims — the
    provider was never called, or the journal is not where the config says.
    That deserves the same named failure as a transport error rather than a
    CalledProcessError traceback.
    """
    try:
        return compose("exec", "-T", service, "cat", path)
    except subprocess.CalledProcessError as error:
        detail = (error.stderr or error.stdout or "").strip()
        fail(f"could not read {path} from {service}: {detail[:300]}")


def journal() -> tuple[str, list[dict]]:
    """The journal as written and as parsed.

    The forbidden-substring check runs against the bytes on disk rather than
    a re-serialisation of them, because escaping is exactly where a leaked
    value would hide from a round-trip.
    """
    raw = read_file("gateway", "/var/lib/tessera/audit.jsonl")
    return raw, [json.loads(line) for line in raw.splitlines() if line.strip()]


def running_services() -> list[dict]:
    """`compose ps`, normalised across Compose versions.

    Some releases emit one JSON object per line and others a single array, so
    the shape is decided at runtime rather than assumed from the version this
    happened to be written against.
    """
    out = compose("ps", "--format", "json").strip()
    if not out:
        return []
    try:
        data = json.loads(out)
    except json.JSONDecodeError:
        data = [json.loads(line) for line in out.splitlines() if line.strip()]
    return data if isinstance(data, list) else [data]


def main() -> None:
    restored = ask()

    # 1. The provider saw placeholders and no values.
    received = read_file("mock-provider", "/received/received.json")
    if IBAN in received or EMAIL in received:
        fail("a submitted value reached the provider")
    minted = MINTED_IBAN.search(received)
    if not minted:
        fail(f"the provider did not receive a masked IBAN; it got: {received}")
    placeholder = minted.group()

    # 2. The client got its values back. The stand-in echoes the placeholders
    #    it was given, so a restored answer names the real IBAN.
    if IBAN not in restored:
        fail(f"the client did not get the value restored; it got: {restored}")

    # 3. The journal recorded the request and quotes nothing.
    raw, lines = journal()
    events = [line["event"] for line in lines]
    if events[-2:] != ["masked", "outcome"]:
        fail(f"expected a masked line then an outcome line, got: {events}")
    # The placeholder is the one this run actually minted, not a guessed
    # number: the counter is shared across entity types, so a detector that
    # emitted the email span first makes the IBAN [IBAN_2] and a hardcoded
    # "IBAN_1" becomes a line that reads well and can never fail.
    for forbidden in (IBAN, EMAIL, placeholder.strip("[]"), "sk-smoke"):
        if forbidden in raw:
            fail(f"the journal carries {forbidden!r}")
    if lines[-1]["result"] != "completed":
        fail(f"the request did not complete: {lines[-1]}")

    # 4. The detector is not reachable from the host. The topology claims it,
    #    and compose is asked directly rather than probed on a fixed port: a
    #    detector published to an ephemeral host port is just as exposed, and
    #    an unrelated service already holding that port would otherwise read
    #    as a pass.
    #    Both checks are kept: `ps` sees a published port that the probe would
    #    miss on an ephemeral host port, and the probe sees an exposure that
    #    `ports:` does not describe — `network_mode: host` publishes nothing
    #    and exposes everything.
    try:
        urllib.request.urlopen("http://127.0.0.1:8000/health", timeout=3)
    except Exception:
        pass
    else:
        fail("the detector answers on the host at 127.0.0.1:8000")

    running = running_services()
    detectors = [s for s in running if s.get("Service") == "detector"]
    if not detectors:
        # Otherwise a renamed service turns this assertion into an empty loop
        # that reports the topology is sound because it looked at nothing.
        fail(f"no detector service is running: {[s.get('Service') for s in running]}")
    for service in detectors:
        published = [p for p in service.get("Publishers") or [] if p.get("PublishedPort")]
        if published:
            fail(f"the detector is published to the host: {published}")

    # 5. The journal and its salt survive a container recreation together.
    #    Separated, the gateway refuses to start — the failure a first user
    #    would find for us.
    #
    #    Recreated rather than restarted: `restart` reuses the container and
    #    therefore its writable layer, so a journal written outside any volume
    #    survives it and the check passes while proving nothing. Recreation is
    #    also the real event — an image rebuild or a compose edit — that a
    #    misplaced mount loses the journal to.
    before = len(lines)
    compose("up", "-d", "--force-recreate", "--no-deps", "gateway")
    for _ in range(30):
        try:
            urllib.request.urlopen(f"{GATEWAY}/health", timeout=2).read()
            break
        except Exception:
            time.sleep(2)
    else:
        fail("the gateway did not come back after being recreated")
    after = len(journal()[1])
    if after < before:
        fail(f"the journal lost lines when the container was recreated: {before} then {after}")

    print(f"smoke: ok ({after} journal lines, nothing quoted)")


if __name__ == "__main__":
    main()
