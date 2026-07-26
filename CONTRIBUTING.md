# Contributing to Tessera

Thank you for considering a contribution.

## Developer Certificate of Origin (DCO)

This project uses the [Developer Certificate of Origin, Version 1.1](https://developercertificate.org/).
Every commit must be signed off:

```
git commit -s
```

which adds a `Signed-off-by: Your Name <your@email>` trailer. By signing off you certify
that you have the right to submit the contribution under the project's license
(Apache-2.0).

## Ground rules

- **Never commit real personal data.** Test fixtures use synthetic values only
  (generated identifiers with valid checksums are fine — they must not belong to a real
  person). The annotated evaluation corpus is private by design and must never enter
  this repository.
- **Identifier catalog is data.** New country identifiers (PESEL, DNI, BSN, …) are added
  in the YAML catalog with a pluggable validator — no changes to the engine required.
- **Tests first.** Detection changes must come with corpus/snapshot test updates; a
  recall regression on Tier 1 identifiers fails CI.

## Development

```
cd detector
uv sync
uv run pytest
uv run ruff check .
uv run mypy src
```
