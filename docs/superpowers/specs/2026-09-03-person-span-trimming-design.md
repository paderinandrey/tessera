# Trimming what a `PERSON` span swallows in front of the name — design

Closes #20.

> Decisions were taken without the user in the room, on a standing instruction.
> The one worth their attention is the last section: this list will lag, and the
> design says how it fails when it does.

## The defect

A session keys on exact value equality, so `Dr. Martina Weber` in turn one and
`Frau Martina Weber` in turn two are two values, two placeholders, and one
person presented to the model as two. Cross-turn coreference is what sessions
exist to buy, and this breaks it for the type where it matters most.

The gateway is behaving as specified. The span boundaries are wrong.

## Measured, because the issue's proposed fix would have covered a fifth of it

Of 76 annotated `PERSON` entities in the public corpus: 46 exact, **19
over-captured**, 11 missed. Every over-capture is at the **front**; there is no
trailing over-capture at all.

| swallowed | count |
|---|---|
| `Kunde` | 4 |
| `Mandant` | 4 |
| `Madame` | 2 |
| `Le salarié` | 2 |
| `Der Mandant` | 2 |
| `Der Kunde` | 2 |
| `Le client` | 1 |
| `Sehr geehrter Herr` | 1 |
| `Herr` | 1 |

The issue proposed a stop-list of honorifics — `Dr.`, `Prof.`, `Herr`, `Frau`,
`M.`, `Mme`. That reaches **4 of the 19**. Fifteen are **role nouns**, usually
behind an article.

**And it is `PERSON` alone.** `LOCATION` over-captures 0 of 8, `ORG` 0 of 4,
every other type 0. So this is not a general boundary problem to be solved
generally; it is one type, and the rule belongs to that type.

## Two alternatives, tested and rejected

**A tighter label.** `person` is what the catalog asks for. Every reformulation
trades the over-capture for recall, and badly:

| label | exact | over | missed |
|---|---|---|---|
| `person` | 53 | 18 | **5** |
| `person name` | 41 | 8 | 27 |
| `given name and surname` | 35 | 3 | **38** |

Over-capture 18 → 3, misses 5 → 38. The over-capture is the price of the
recall.

**Two labels, taking the nested boundary.** Keep `person` for recall and, where
a `person name` span nests inside one, use the inner bounds — a narrow label can
then only trim, never add, so a low threshold for it is free. It works and it is
small: 18 → 15, no recall cost, four spans trimmed. Lowering its threshold does
not help; at 0.02 the longest nested span becomes the role phrase itself and it
starts preferring `Le client` over `Marty`.

Kept as a note in the issue rather than built. Three of eighteen is not worth a
second inference label in every pass.

## The change

**A list of words a `PERSON` span may not begin with, in the catalog, applied
after the model and before resolution.** Leading tokens are dropped while the
first one matches; the rest of the span is untouched.

Data, not code, because the repository already treats catalogs that way and
because this list is the part that will need adding to. It lives on the
`PERSON` entry in `ner.yaml`, since no other type needs one.

Three kinds of word, all present in the measurements:

- **articles** — `der`, `die`, `le`, `la`, so `Der Kunde Karz` loses both words;
- **role nouns** — `kunde`, `mandant`, `salarié`, `client`, and their feminine
  and plural forms;
- **honorifics and salutations** — `herr`, `frau`, `madame`, `monsieur`, `dr`,
  `prof`, and the German salutation words `sehr`, `geehrter`, `geehrte`.

Matched case-insensitively and after stripping a trailing `.`, so `Dr.` and
`dr` are one entry.

**One word per entry, enforced by the loader.** The rule walks a span a token at
a time, so `Der Kunde` written as a single entry would never match anything and
would sit in the catalog looking as though it did. That is worse than an absent
entry, because it reads as coverage — the same rule this repository applies to a
guard that guards nothing. The words go in separately and the walk trims them in
sequence.

## What it cannot do, said plainly

**Trimming makes the mask narrower**, so a trimmed word reaches the provider.
That is correct — `Der Kunde` and `Sehr geehrter Herr` are not personal data —
and it reduces over-masking, which REQ-38 counts as the irritation cost.

**A span is never trimmed to nothing.** If every token matches, the span is left
as it came: a rule that can empty a span is a rule that can unmask a name.

**The list will lag.** Role nouns are open-ended in a way titles are not —
`Patient`, `Mandantin`, `Versicherte`, `Bénéficiaire`, one per domain a client
works in. A missing entry leaves today's behaviour, which is an over-wide mask
rather than an exposure, so it **fails safe**; but it fails, and calling
twenty-three words a solution to an open class would be the wrong claim to put
in a design document.

## Testing

- the two turns from the issue reduce to **one string**, which is the defect:
  the mapping keys on exact value equality, so equal strings are one placeholder
  and nothing further needs proving at the gateway;
- each shape in the measurement table above trims to the annotated name;
- a span made entirely of listed words survives untrimmed;
- a name that *is* a listed word — a person called `Herr` — is not trimmed away,
  which is the same rule as the one above and worth its own case;
- the corpus's `PERSON` exact-bound count rises and its missed count does not.

Mutations: remove the trim and the two-turn test fails with the honorifics still
attached; let the walk stop at the last space rather than testing the last token
and `Der Kunde` trims to `Kunde`, which the never-empty case catches; drop the
single-word check on catalog entries and the loader accepts `Der Kunde` as one
entry that can never fire.
