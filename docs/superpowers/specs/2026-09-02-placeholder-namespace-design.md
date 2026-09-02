# A per-session namespace for issued placeholders — design

Closes #32. Completes the half #31 left open deliberately.

> **Decisions in this document were made without the user in the room**, on a
> standing instruction to take the recommended option. Four are worth their
> attention on review and are marked **[decided]** where they appear: the salt's
> length, what the journal records, which mechanisms are deleted rather than
> kept, and what gets measured before any of it ships.

## The question that has no answer

The gateway must tell a token it issued from one the caller wrote, because it
restores the first and must leave the second alone. Today it answers by
**shape**, and shape cannot answer it across turns:

1. turn one detects `Martina Weber` and commits `Martina Weber → [PERSON_1]`;
2. turn two carries the caller's **own** literal `[PERSON_1]`;
3. `reserve_literals` yields to whoever holds the key, and the key was taken by
   an allocation made before the literal existed;
4. the provider echoes it, restoration rewrites it to `Martina Weber`, and the
   client's agent may act on an argument the caller never supplied.

No ordering reaches this. The allocation predates the literal by a whole turn.

Five mechanisms now answer that one question by shape — `is_placeholder`'s
grammar, `pieces`' advance-one-byte rule, `key_is_unserveable`'s two arms,
`reserve_literals`' self-mapping, and #29's request-wide reservation pass. Each
closed the site it was written for. **A sixth would close a sixth site.**

This slice stops asking about shape. An issued token carries a component the
caller cannot have written, and provenance becomes a lookup.

## The shape

```
[PERSON_1]          a caller's literal — recognised, never restored
[PERSON_1.3f7a2b91c4d5] issued by this session — restored
[PERSON_1.0a1b7c33e2f1] issued by some other session — recognised, never restored
```

The grammar accepts both forms. **Recognising a token and owning it are
separate questions**, and keeping them separate is the point: a caller's literal
must still be *seen* — `pieces` must yield it, the journal must count it, the
streamed buffer must hold it — so that it can be left alone deliberately rather
than by accident.

`placeholder_type` keeps its meaning and gains nothing. A new predicate answers
ownership by comparing the salt against the session's, and it is the only place
that answers it.

### Why a suffix and not a wider number space

Randomising the number instead — `[PERSON_83417]` — needs no grammar change and
touches no mechanism, which is a real advantage and was the alternative
considered longest. It loses on two counts. The index stops meaning *the Nth
distinct value in this session*, which is the property the audit journal is read
for; and it puts the entropy in the position a model uses for coreference, where
`[PERSON_83417]` and `[PERSON_83418]` are harder to tell apart than `[PERSON_1]`
and `[PERSON_2]`. A uniform suffix leaves the index small and the noise in one
place.

### Namespace width, and why the first answer was wrong **[decided, revised]**

**Twelve lowercase hex characters — forty-eight bits.**

The first draft said sixteen bits, on the argument that the security boundary is
the credential: a session is keyed by *(credential, session id)*, and
`a_guessed_session_id_returns_no_other_callers_value` holds that a different key
reaches nothing.

**That argument was wrong, and wrong in a way worth recording.** The property
this namespace protects is the integrity of *the caller's own text*, and the
caller holds the credential. The adversary is inside the boundary the argument
appealed to. With sixteen bits, a request can carry every one of the 65 536
candidates in about a megabyte of text; if the provider echoes them, the one
that matches an existing mapping is restored, and the argument-substitution
defect is back — **after** `written` has been deleted, which is what made the
enumeration worth doing.

Forty-eight bits is not enumerable within any request budget, and that is what
pays for deleting `written`. The token grows to twenty-three bytes for a typical
value, which the streamed bound absorbs and a model reads as one opaque suffix.

### Derived, not minted **[decided, revised]**

**The namespace is `SHA-256(session key)`, truncated — not a value minted per
`Mapping`.**

Minting per mapping fails on the path that produces the tokens this slice
exists to refuse. Eviction removes a whole session; the next request with the
same id constructs a fresh `Mapping`, which would mint a *different* namespace.
A token from the evicted incarnation would then read as a stranger's, be left
rather than refused, and reach the client — the exact case Task 6 claims to
close, failing quietly.

Derivation from the session key is stable across eviction and recreation, and
`SessionKey` is already a salted fingerprint: `session::salt()` is thirty-two
random bytes per process, mixed into the credential hash. So the namespace is
unguessable for the same reason the key is, and the comment on that salt already
carries the argument — *"sessions do not survive a restart in any case, so a
salt that does not either costs nothing."*

A request with **no** session still mints randomly. It lives one request, there
is no recreation to be stable across, and nothing outlives it to be refused
later.

## What it costs, before it is built

The issue is unusually specific about the price, and this design does not
discount it.

### The model sees a stranger token, and that is measured first **[decided]**

**Task 1 measures it. Nothing else starts until the numbers exist.**

What is measurable here: our own detector reads text that carries placeholders
from earlier turns, and a salted token could change what it finds — a new span,
a lost one, a shifted boundary. The corpus and `evaluation/evaluate.py` answer
that directly.

What is not measurable here: whether a provider's model answers a salted prompt
worse than an unsalted one. There is no response-quality harness in this
repository and building one is not this slice. **That risk is accepted, not
measured**, and the design reduces it rather than testing it — the suffix is
short, uniform across a session, and leaves the index intact, so coreference
reads the same way. Anyone who later disagrees should reach for the fallback in
the next section rather than re-litigating it here.

### The fallback, named now rather than improvised later

If Task 1 shows the detector's numbers move outside the gates
`evaluation/evaluate.py` already enforces, **the slice stops and reports**. It
does not proceed with a smaller salt or a different delimiter chosen to make the
numbers come back — that is fitting the design to the measurement after seeing
it. The recorded outcome is then a decision for a human: accept the detection
cost, change the detector's handling of bracket tokens, or leave #32 open.

### The streamed bound has one byte of headroom, and this needs five

`MAX_ENTITY_TYPE` is 40 and `stream::MAX_HELD` is 64, and the comment on the
former does the arithmetic: `[` + 40 + `_` + 20 digits + `]` is 63 bytes. The
bound is not a guess — it is what lets the streamed buffer release an unclosed
`[` as ordinary text without ever orphaning a real token.

A twelve-character namespace and its delimiter make the worst case 76. **`MAX_HELD`
becomes 80**, and the arithmetic in both comments is restated rather than
adjusted, because the next person to add a component to the token will read
those comments and not this document.

### The journal records the unsalted token **[decided]**

`[PERSON_1]`, not `[PERSON_1.3f7a2b91c4d5]`.

The salt is transport. It exists so the response path can answer a question the
journal never asks, and recording it would make two lines from two sessions
incomparable for no gain — the journal is read to compare, and #18 built it for
that. It would also put a per-session value into a log for no reason, and a
value in a log is a value that leaks.

The journal's own grammar (`audit::is_entity_type`) is unchanged by this, which
is the check that the decision is the cheap one.

## What the certainty buys

Two things, and the second is the reason #31 was allowed to ship incomplete.

**The cross-turn corruption closes.** A caller's `[PERSON_1]` carries no salt,
is not ours, and is left alone — in every field, on both paths, with no ordering
argument required.

**#31's residual closes.** An undescribed field carrying a token that *is* ours
but has no mapping — an evicted session, a stale turn — can now refuse instead
of leaving it, because "ours" is finally decidable. The README's promise loses
its qualification: a placeholder this gateway issued does not reach the client,
full stop, rather than *from a field the gateway describes*.

## `Provenance` collapses, and that is the shape of the win

#31 introduced `Provenance` — `issued ∧ ¬written` — because a lookup is not
provenance while a session outlives a request. Its own doc comment names this
slice as what separates the two sets.

With a salt the lookup **is** provenance. A token carrying this session's salt
was issued by this session; a caller cannot have written it; the two sets stop
overlapping by construction. `restorable` becomes *carries our salt and has a
mapping*, and `issued`, `written`, the per-request sets and the request-wide
walk that fills them all have nothing left to decide.

**This buys coverage as well as simplicity, which was not in the issue.** Today
the sweep can restore only what *this request* issued, because that is the only
set it can trust. A value masked in turn one, not mentioned in turn three, and
echoed by the model in turn three's response is left as a placeholder — ours,
mappable, and not restored, because turn three's `issued` does not contain it.
Ownership by salt restores it. That case is common in a long conversation and is
worth a test of its own.

## What is deleted **[decided, revised twice]**

**Only `Provenance`.** Its two sets are what ownership replaces, and its own doc
comment names this slice as what separates them.

The first draft listed four more, on this repository's rule that a guard which
guards nothing reads as coverage. Two review rounds found that three of them
guard something after all, and the pattern in the mistakes is worth more than
the list:

- **`key_is_unserveable`'s first arm** refuses any exact placeholder-shaped
  property key. The arm that would remain refuses only keys this mapping maps,
  so deleting it moves a client from a refusal to a model-chosen property name.
- **`reserve_literals`** keeps a caller's literal self-mapped, and the *strict*
  path depends on that: it looks the token up unconditionally and raises
  `Unknown` on a miss, so a caller's `[PERSON_1]` echoed into a described slot
  would become a 502 where it is a successful unchanged response today. The
  sweep is indifferent — a self-mapped literal carries no namespace, so `owns`
  is false and it is left either way.
- **The skip-taken-numbers loop** genuinely guards nothing once namespaces
  exist, and stays anyway: removing it while `reserve_literals` still writes
  into the same map makes its safety depend on two things at once, and this
  design has now been wrong twice about which mechanism guards what.

**The rule the two rounds produced:** *"this exists to answer a question I am
replacing" is a claim about the mechanism's history, not about its behaviour.*
Both wrong deletions were mechanisms built for the shape question that had
quietly acquired a second job. Deleting them is a follow-up issue, argued
against real traffic rather than against a plan.

What is **not** deleted for a different reason: `pieces`, `is_placeholder` and
`placeholder_type` recognise tokens, which is still needed and now needed twice
over — a stranger's token must be recognised precisely so it can be left.

## Tasks

1. **Measure.** Detector behaviour on text carrying salted tokens, against the
   corpus, with the numbers recorded. Gate: the existing evaluation gates. Stop
   and report on failure.
2. **Mint salted tokens.** Per-session salt, grammar accepts both forms,
   ownership answered in one predicate.
3. **Raise `MAX_HELD` to 80** and restate the arithmetic in both comments.
4. **Provenance becomes a lookup.** Replace the shape-based answers; delete the
   mechanisms that only served them.
5. **Journal records the unsalted token.**
6. **Spend the certainty.** Undescribed fields refuse on an ours-but-unmappable
   token; the README promise loses its qualification, everywhere it is stated.

## Testing

Every invariant is proved by mutation: break it, run the *named* test, check
*why* it failed, restore by inverse text substitution. The three that carry this
slice:

- a caller's literal, written in turn two after turn one issued the same
  bare token, comes back **unchanged** — the defect, end to end through the
  router, which is how it was found;
- a token carrying **another session's** salt is left alone, so ownership is not
  merely "well-formed and salted";
- a token carrying **this** session's salt with no mapping **refuses** in an
  undescribed field, which is the residual #31 left and the promise this slice
  restores.
