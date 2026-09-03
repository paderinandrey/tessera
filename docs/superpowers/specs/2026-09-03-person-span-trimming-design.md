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

**Articles are a separate list and are never trimmed alone.** `Le` is a
Vietnamese family name and `Das` a Bengali one, so trimming a leading article by
itself sends a real name component to the provider in clear — `Le Thi Mai`
arriving as `Le [PERSON_1]`, which is worse than the over-wide span the rule
exists to narrow. An article goes only in front of a word that goes anyway,
which is the only shape the corpus ever shows. Found in review, and it was a
defect this design introduced rather than one it inherited.

**And a span is never trimmed down to punctuation.** The model puts trailing
punctuation inside a span often enough that `Herr !` reaches the rule, and the
never-empty clause counted *tokens* — so the `!` looked like remaining content,
the trim kept it, and `Herr` went to the provider in clear. The clause asks
about content now: what survives must carry a letter or a digit.

Two more edges, both from the same review and both real:

- **the span must start on a word boundary**, and *the caller* says whether it
  does. A token window can begin inside a word, and this function is handed the
  window rather than the document — so offset zero looks like a boundary when it
  is the tail of `Alexander`. A caller that does not know says nothing and
  nothing is trimmed;
- **whitespace is a run of any Unicode space.** Scanning for a single `" "` made
  a second space produce an empty token, which is on no list, so the walk
  stopped there — and an all-listed `Der  Kunde` slipped past the never-empty
  guard and exposed `Der`.

Three kinds of word, all present in the measurements:

- **articles** — `der`, `die`, `le`, `la`, trimmed only ahead of one of the
  others, so `Der Kunde Karz` loses both words and `Le Thi Mai` loses none;
- **role nouns** — `kunde`, `mandant`, `salarié`, `client`, and their feminine
  and plural forms;
- **honorifics and salutations** — `herr`, `frau`, `madame`, `monsieur`, `dr`,
  `prof`, and the German salutation words `sehr`, `geehrter`, `geehrte`.

Matched case-insensitively and after stripping a trailing `.`, so `Dr.` and
`dr` are one entry.

**A word may not appear in both lists.** An entry in both is subtracted out of
the article safeguard and trimmed unconditionally — `le` in both makes
`Le Thi Mai` lose its family name, which is the disclosure the split exists to
prevent. A catalog mistake that disables a safety rule stops the service rather
than the name.

**One word per entry, enforced by the loader.** The rule walks a span a token at
a time, so `Der Kunde` written as a single entry would never match anything and
would sit in the catalog looking as though it did. That is worse than an absent
entry, because it reads as coverage — the same rule this repository applies to a
guard that guards nothing. The words go in separately and the walk trims them in
sequence.

## The article list is the risky half, and dropping it is a catalog edit

Every review finding on this change has circled the article safeguard: an
article trimmed alone, a word listed in both lists, a span left holding
punctuation, a window starting mid-word. The rule for role nouns and titles has
not been challenged once.

So the fallback is worth measuring rather than describing, and it is cheap:

```
articles kept      PERSON exact 73   wider 0   unmasked 3
articles dropped   PERSON exact 66   wider 7   unmasked 3
```

**Dropping them costs seven spans and no exposure.** Twelve of the nineteen
over-captures still resolve — including both turns of the case in the issue,
which carry no article — and the entire class of risk goes with the list:
`Le` cannot be trimmed off a Vietnamese name if `le` is not there to be trimmed.

**It is a catalog edit, not a code change.** Deleting the `trim_leading_articles`
block leaves a rule that behaves correctly with an empty set, which the unit
tests cover. Whoever operates this can take the safer half without a release,
and that reversibility is the reason the list ships enabled: the guards around
it are tested and mutation-proved, and the way back is one block of YAML.

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
