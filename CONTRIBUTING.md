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

CI checks **every** commit in a pull request, merge commits included, and requires a
trailer naming the commit's own author — the certification is about who wrote it, so a
sign-off by somebody else is not it. To add one to commits you have already made:

```
git rebase --signoff origin/main
```

`git rebase --signoff` signs as the **committer**, so it only helps where you are also
the author. A commit written by someone else has to be signed off by them; rebasing it
adds *your* trailer, leaves their authorship, and the check will still reject it.

The check runs on the pull request's commits rather than on `main`, because that is where
the contribution is. Squash-merge promotes the trailers into the merge commit's own
trailer block — verified on `063aa83`, a two-commit squash whose `Signed-off-by` GitHub
lifted into the final block — so the attestation normally reaches `main` too. That is a
property of the default squash message and not a guarantee: an author who rewrites the
message by hand at merge time can drop it, and the attestation then lives only in the
pull request.

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
