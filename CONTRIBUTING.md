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

CI checks every commit in a pull request and requires the trailer to name the commit's
own author — the certification is about who wrote it. To add it to commits already made:

```
git rebase --signoff origin/main
```

The check runs on the pull request's commits rather than on `main`, because that is where
the contribution is. Squash-merge composes its message from them, so the trailer reaches
`main` as well.

> Commits merged before this check existed do not carry the trailer, and the history is
> deliberately not rewritten to add one: a sign-off is an attestation by a person, and
> backfilling it would be asserting something on their behalf after the fact.

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
