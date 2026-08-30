# Handoff: building a frontend against Tessera's detector

Written for an agent starting frontend work with no context on this repository.
It tells you what exists, what the contract is, what will confuse you in the
first hour, and what is not yours to change.

**Read `docs/api/openapi.json` as the contract.** Everything below is
orientation. Where this document and that file disagree, the file is right — it
is generated from the server and CI fails if it drifts (`make openapi`, then
`git diff --exit-code docs/api`).

## What the product is, in one paragraph

Tessera pseudonymizes personal data before it reaches an LLM provider. A gateway
sits in front of the provider's API, replaces values it finds with placeholders
like `[PERSON_1]`, forwards the masked request, and restores the real values in
the response. Sold to regulated European buyers, for whom the audit journal is
part of the product.

This paragraph used to end "the model never sees the data; the client never sees
a placeholder", and both halves were false. Upward: no *text* the gateway scans
reaches the provider unmasked, but image and audio parts are forwarded untouched
— nothing here reads pixels, so a photograph of an identity document goes up as
the client sent it. Downward: a placeholder issued by the gateway does not reach
the client from a field the gateway describes, and elsewhere everything a
request issued and the caller did not write is restored, *except where restoring
it would drop something the provider sent*, which is served exactly as it came.
That exception has two shapes and one rule — an object whose restored keys would
collide, and a string that is itself a serialized document carrying two members
of the same name — and in both the gateway cannot put the value back without
losing a field, so it leaves the bytes alone and gives up the restoration.
None of that is your surface — you are building against the detector, for the
reasons below — but it is the sentence a buyer will quote back, so do not
restore the short version of it, and do not drop a clause from it. `README.md`
states both directions with their qualifications and is the place to correct if
they change.

## Which service you talk to, and why not the other one

**Talk to the detector. Do not build against the gateway.**

The gateway has three routes — `/health` and two provider passthroughs
(`/v1/chat/completions`, `/v1/messages`). Its entire interface is *being an LLM
API*: it carries the caller's provider credentials, holds per-session mapping
state, and its responses are provider responses. There is nothing there for a
browser.

The detector is a small FastAPI service with a documented, versioned contract
and no credentials, no sessions and no state. That is the surface a frontend can
stand on.

## The contract

**`POST /detect`**

```json
{ "text": "Martina Weber lives in Bern", "layers": ["deterministic", "ner"] }
```

`text` is required and must be non-empty. `layers` is optional; omit it to run
everything. It may only *narrow* what runs and must always include
`deterministic` — the catalog layer costs microseconds and its checksum-backed
spans are what stop a model guess from displacing a real identifier.

```json
{
  "spans": [
    { "entity_type": "PERSON", "start": 0, "end": 13,
      "confidence": 0.91, "recognizer": "ner:gliner", "tier": 2,
      "boosted": false }
  ],
  "layers_run": ["deterministic", "ner"],
  "version": "…"
}
```

- `start` / `end` are **character** offsets into the original text, not bytes.
  Highlighting with byte offsets will misplace every span after the first
  non-ASCII character, and this is a product about European names.
- `recognizer` is the field that makes the UI honest — see the next section.
- `tier` is 1 for catalog identifiers, 2 for quasi-identifiers, 3 for GDPR
  Article 9 special categories.
- `version` is a digest of the weights *and* the catalogs. It changes whenever
  the same text would detect differently. Anything you cache must be keyed by
  it.

**`GET /health`** returns `{ "status": "ok", "ner": bool, "ner_off_reason": str|null }`.
Show `ner: false` in the UI. See "What will confuse you in the first hour".

**Errors** return `{ "detail": "…" }` — a reason, never the submitted text. That
is deliberate and applies to your UI too: do not echo scanned text into an error
banner or a log.

## The one thing the UI must get right

**Detection has two layers, and collapsing them into one badge throws away what
the product is trusted for.**

- **Deterministic** — a regex finds a *candidate*, and a named checksum
  validator decides. A candidate that fails its validator produces no span at
  all. `4111111111111111` is a credit card because it passes Luhn;
  `9007199254740991` is not, because it fails. Eight types: `CH_AVS`,
  `CREDIT_CARD`, `DE_STEUERNUMMER`, `DE_STEUER_ID`, `EMAIL`, `FR_NIF`,
  `FR_NIR`, `IBAN`. Catalog: `detector/src/tessera_detector/catalog/identifiers.yaml`.
- **NER** — a zero-shot model (`gliner_multi-v2.1`, pinned by revision, run
  under onnxruntime) reading meaning rather than form. Fourteen types: `PERSON`,
  `LOCATION`, `ORG`, and eleven Article 9 categories with no format at all —
  health, ethnicity, religion, political opinion and the rest. Catalog:
  `detector/src/tessera_detector/catalog/ner.yaml`.

`recognizer` tells you which fired: `catalog:credit_card` versus `ner:gliner`.
A buyer evaluating this needs to see the difference — "this passed a checksum"
and "a model thought this looked like a person" are different claims, and
showing them identically is the one design mistake that would matter.

On a partial overlap the catalog wins: identifiers carry specificity 40–90,
model guesses 10–35.

## What will confuse you in the first hour

**`ner: false` is not a bug.** The model weights are hundreds of megabytes and
are never committed. Without them the service starts and runs the deterministic
layer alone, reporting `ner: false` with a reason. You will get spans for cards
and IBANs and nothing for names, and conclude the model is broken.

Fix: `make model` downloads the pinned weights to a user cache, and the `ner`
dependency group must be synced (`uv sync --project detector --group ner --group serve`).

**Or skip all of it and use compose.** `docker-compose.yml` builds the detector
and exposes it on port 8000 with a healthcheck. For frontend work this is the
shorter path — you do not need the gateway service at all.

## Not yours to change

- **The detector's API.** If the UI wants a field the response does not carry,
  that is a change to the detector, and it goes through the same review as
  anything else on the backend side. Raise it; do not add it. `docs/api/openapi.json`
  is CI-diffed precisely so this cannot happen quietly.
- **The catalogs.** `identifiers.yaml` enforces that a high-specificity entry is
  checksum-backed; the loader fails otherwise. `ner.yaml`'s header carries a
  warning about adding digit-shaped entities that is about gateway behaviour,
  not about the file.
- **Anything under `gateway/`.** Different surface, active work, separate branch.

## What is not built and what that means for you

**Nothing is deployed anywhere.** There is no hosted instance, no auth story,
and no decision about where a frontend would live or whether it is public. If
the answer is "public demo", then whose detector it calls and what happens to
the text people paste into it are product questions, not implementation details
— settle them before writing the page, not after.

**Do not build an operator console or a journal viewer yet.** The audit
journal's schema has changed twice in the last week: a field was added, two
counters were redefined, and one changed its unit. It will settle; it has not
yet.

## Working agreement

Separate branch, separate PR. The backend side is mid-slice on
`feat/openai-response-path` and touches `gateway/` and `detector/src/`; a
frontend under its own directory will not collide.

This repository reviews specs before code and proves invariants by mutation —
break the thing the test is for, watch the *named* test fail, check *why*, put
it back. Worth knowing before your first PR arrives; it is the house style, not
ceremony.
