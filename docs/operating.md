# Operating it

What the two commands in the README leave out: why the weights download is separate, what the gateway publishes and to whom, and which volumes must be backed up together.

**Download the weights first.** The detector builds its pipeline once, at
startup, because loading the model takes seconds and paying that per request
would be absurd — so a detector that started without weights stays
deterministic-only until it is restarted, however much you download afterwards.
Adding them to a running stack therefore needs `docker compose restart detector`,
and skipping that restart is the one way to end up with a successful 2 GB
download, a gateway that looks installed, and names, places and health mentions
still reaching the provider unmasked.

The gateway does serve before the weights are there, deliberately. Without them
the detector runs its deterministic layer alone — checksum-validated
identifiers, the layer that scores 1.000 — and a partial install stays visible
rather than silent: the detector's own `GET /health` reports `ner: false` and
why, though it answers only on the compose network, never on the host. The
download is a separate command on purpose: a gateway that belongs inside your
perimeter should not reach the internet on its own, and a first start that
blocks for minutes is indistinguishable from one that has hung. It is the same
download `make model` does for a local, non-containerized detector; here it lands on a named volume,
so it is a one-time cost per host rather than a cost of every `up`.

Only the gateway is published, and on `${TESSERA_PORT:-8080}` rather than a
fixed `8080` — a variable, because a host that already has something bound to
8080 should not have to edit a file to try Tessera; set `TESSERA_PORT` before
`up` to use another port instead. The host address is a variable too, and
defaults to loopback: until `accepted_credentials` is set the gateway serves
anyone who can reach it, forwarding whatever credential arrives, so reaching it
from beyond this host is a deliberate act — `TESSERA_BIND=0.0.0.0` — and never a
side effect of the default. It holds no key of its own, so what you would be
publishing is not your provider credit but a relay out through your egress and
into a journal whose worth is that it records your traffic and not a stranger's:
strangers can fill the session table until legitimate callers are refused with a
503, and anyone who already holds one of your callers' keys can guess a session
id — they are chosen by the client and need not be secret — and read that
conversation's real values back out of its table, which going to the provider
directly would never have given them.

**So list who you serve before you publish it.** `accepted_credentials` takes
the SHA-256 digest of each credential your callers already send — `printf '%s'
"$KEY" | shasum -a 256` — and a request whose credential is not on the list is
refused with 401 before the body is walked, before the detector is called and
before anything goes upstream, so it costs neither detector time nor anybody's
tokens. The refusal is attributed in the journal under `caller_not_served`, so a
run of them tells you whether one client has the wrong key or somebody is trying
keys. It narrows who can reach the mapping table; it does not make the table
safe to reach, because a caller who *is* served can still be handed a value they
did not send — see [the session table](sessions.md) and issue #32. An
authenticating proxy in front remains worthwhile where you want a credential of
your own rather than the provider's.
The detector answers on the compose network and nowhere else: `POST /detect`
takes arbitrary text and authenticates nobody, so exposing it would be a way to
run text through the model outside the gateway, and therefore outside the audit
journal.

The journal and its salt share the `audit` volume and must stay together — a
journal with records whose salt has gone missing refuses to start rather than
silently renumbering every tenant beneath you. Back up that volume, not just
the file. `docker compose down` stops the stack and keeps the journal, its
salt and the downloaded weights; add `-v` only when you mean to discard them
— journal and salt together, since one without the other is what refuses to
start back up.
