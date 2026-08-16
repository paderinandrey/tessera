# Entity types come from a known set, not a grammar

## The problem

`Mapping::placeholder_for` validates the detector's `entity_type` against a
grammar and nothing else: non-empty, at most `MAX_ENTITY_TYPE` characters, and
`[A-Z_]` throughout. A detector that returns an uppercase span value as its own
type — `entity_type: "WEBER"` for a span covering `WEBER` — passes that check,
and the gateway builds `[WEBER_1]` and **sends it to the provider**.

The value leaves the perimeter inside the placeholder's name. Everything else
about the request is masked correctly; the leak rides in the one field nobody
thought of as content.

Found by Codex on PR #18, against the audit journal's `types` keys. The journal
has the same weakness and stays inside the perimeter; the placeholder does not,
which is why this design is about `mapping.rs` and the journal's check is only
mentioned where the two interact.

### Why the grammar was never enough

The check was written to answer a different question — whether a type can be
written as a token restoration will recognise — and it answers that one
correctly. It was never a check on what the string *means*. Syntax cannot
distinguish a type name from a value that happens to look like one, and all-caps
values are ordinary in real documents: surnames on forms, scanned text,
identifiers.

### Threat model

The gateway treats the detector's response as untrusted input arriving over
HTTP. That is not a new position taken for this slice — it is why the validation
exists at all, and SECURITY.md names detection bypasses as in scope.

Reaching that bar requires a detector that returns the value as the type, which
the shipped detector never does. A compromised detector would; so would a
misconfigured NER label set, with no attacker involved at all.

**One consequence follows immediately and shapes the whole design.** If the
gateway asked the detector which types it emits and pinned the answer, a
compromised detector would simply declare `WEBER` a type. Against the threat the
check exists for, that buys nothing. **The vocabulary must come from somewhere
the detector cannot influence at runtime.**

## The decisions

1. **The gateway holds the vocabulary**, as a constant in `mapping.rs`. Not from
   `/detect`, not from a capability endpoint, not from configuration.

2. **The list replaces the grammar** in `placeholder_for` rather than joining it.

3. **An unknown type is masked under a generic name**, `[REDACTED_n]`, not
   refused.

4. **CI fails when the constant and the detector's catalogs disagree.**

### Why not configuration

Making the list operator-extensible does not contradict the threat model — the
detector cannot write the gateway's config either. But it turns a security
control into something one line of TOML can widen, and nobody has asked for it.
A deployment that swaps in its own `/detect` implementation with new types is a
case to handle when it exists, not before.

### Why an unknown type is masked rather than refused

Refusing looks stricter and is not. Confidentiality is identical either way —
the value is masked in both — so the difference is only in what a *new* type
costs. Refusal couples the two versions hard: a detector release with a new type
breaks a working gateway until it is upgraded, and it does so precisely when the
operator is trying to widen detection.

What is lost is that the model no longer sees what was hidden: `[REDACTED_1]`
rather than `[DE_PERSONALAUSWEIS_1]`. That costs answer quality, not protection,
and it is the trade this product already makes elsewhere — over-redaction is the
safe failure.

### Why not extend the OpenAPI contract instead

`docs/api/openapi.json` declares `entity_type` as a bare string, and the README
calls that file "the schema both implementations share". Generating an enum from
the detector's catalogs and having the gateway check against the contract would
give one source of truth rather than two that must agree.

It is the cleaner architecture and it is not worth it yet. The vocabulary does
not churn: the eight deterministic types are fixed by the identifier catalog and
the fourteen NER labels by the model's configuration, and adding either already
requires a detector release. The drift protection below buys the same guarantee
for less machinery. Enriching the contract is worth doing when it earns its own
keep.

## The vocabulary

Twenty-two names, exactly those the detector's catalogs declare.

Eight from `identifiers.yaml` — `IBAN`, `CREDIT_CARD`, `CH_AVS`, `FR_NIR`,
`FR_NIF`, `DE_STEUER_ID`, `DE_STEUERNUMMER`, `EMAIL` — and fourteen from
`ner.yaml`: `PERSON`, `LOCATION`, `ORG`, and the eleven Article 9 categories
`HEALTH`, `BIOMETRIC`, `GENETIC`, `ETHNICITY`, `RELIGION`,
`PHILOSOPHICAL_BELIEF`, `POLITICAL_OPINION`, `POLITICAL_AFFILIATION`,
`TRADE_UNION`, `SEXUAL_ORIENTATION`, `SEX_LIFE`.

`REDACTED` is the gateway's own and is deliberately not in the catalogs: a
detector that returned it would be indistinguishable from the gateway's own
fallback.

### What happens to `MAX_ENTITY_TYPE`

It stays, and changes meaning. It was an input check; it becomes an assertion
about the list itself.

The constant exists because streaming restoration holds back a bounded number of
bytes while a token completes, so a placeholder longer than that bound would be
released as ordinary text and reach the client unrestored. That reasoning is
about `stream::MAX_HELD`, not about what the detector sends, and it survives the
list unchanged — a `debug_assert` that no name in the vocabulary exceeds it.

## Behaviour

A span whose type is in the list masks as it does today.

A span whose type is not in the list masks as `[REDACTED_n]`. The placeholder
counter is shared across types, so `REDACTED` draws from the same sequence: two
unknown-typed values become `[REDACTED_1]` and `[REDACTED_2]` and stay
distinguishable, and one value seen twice keeps one placeholder, as any value
does.

One `tracing::warn!` per request that contained an unknown type, carrying the
count. **Not the name** — the name came from outside the perimeter and is exactly
what this design refuses to write down.

### The journal's own check stays

`Record::detected` already buckets illegible keys under `unvalidated`, and that
check is load-bearing today: `placeholder_for` returns the cached placeholder on
a `by_value` hit *before* validating the type, so a value masked earlier in the
same request carries the detector's string past the mapping untouched.

This design does not remove it. Both checks remain because masking and audit
must not depend on each other — a future change to one should not silently widen
the other. After this slice the journal's check becomes a second line rather
than the only thing between a detector's string and the evidence file.

## Drift protection

A script compares the gateway's constant against the union of the two catalog
files and exits non-zero on any difference in either direction. A CI job runs it.

This is the same shape the repository already uses for the OpenAPI document:
`make openapi` regenerates it and CI fails when it drifts. Adding a type becomes
a deliberate two-file change instead of a silent divergence.

Without it the constant rots at the first type anyone adds, and the same class of
defect returns in a new place.

## Testing

Three, each of which must be able to fail:

- A span with `entity_type: "WEBER"` over the text `WEBER` produces
  `[REDACTED_1]`, and the string `WEBER` appears nowhere in the body sent
  upstream. This is the leak, in the shape Codex reported it.
- Every one of the twenty-two known types still produces a placeholder carrying
  its own name. Without this, the list can be "fixed" into rejecting everything
  and the first test still passes.
- The drift check fails when one name in the constant is changed. A drift check
  nobody has watched fail is a guess.

The second and third are the ones that catch a wrong fix rather than a missing
one, which is why they are here and not in a follow-up.

## Out of scope

Tool traffic (its own slice, and the larger gap). Any change to the OpenAPI
contract. Adding entity types to the detector. Making the vocabulary
configurable. The image policy for unscanned content parts.
