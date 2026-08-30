# Closing OpenAI's response path

Issue #31. The README promises that no placeholder is ever handed to the client
in place of a value. On OpenAI's buffered response path that is false, verified
by driving the router and reading what the client received:

```
choices[].message.refusal                     -> "I cannot help with [PERSON_1]"
choices[].message.annotations[].url_citation.title -> "[PERSON_1] page"
```

`content` restores correctly in both cases, so the client gets a real answer with
this gateway's own token embedded in it.

## The problem is not two fields

`OpenAi::response_pointers` describes two locations and forwards every other one
unrestored. It is an **open** list: whatever OpenAI ships next joins it
automatically, and fixing `refusal` leaves `annotations`, and fixing both leaves
the next one.

Anthropic's response path is closed and OpenAI's is not. **That asymmetry is the
finding.** It is the fourth of its shape in this codebase — one provider given a
treatment the other was not, with nothing comparing them — after the `tools`
array admitted on an Anthropic message, `tool_choice` refused by one provider and
admitted by the other, and the deprecated `function` role walking past a
dispatch. Each was found by an external reviewer rather than by us.

## The cause is one layer down

The gateway has **three restoration policies**, and nobody had compared them
either:

| site | policy |
|---|---|
| a streamed event carrying no slots (`stream.rs`) | restores the **whole event** |
| an error body (`proxy.rs`) | restores the **whole body** |
| a buffered success (`proxy.rs`) | writes **only the described slots** |

The buffered success path is the odd one out. "Only what is described" is why an
undescribed field forwards a token, and closing OpenAI's list would fix the
instance while leaving the policy that produced it.

## Design

**Sweep the original body leniently. Then overwrite each described field with its
strict slot restoration, computed from the same original.**

The change is **purely additive**. Nothing that refuses today stops refusing;
nothing that succeeds today starts failing. Coverage grows and nothing else
moves.

### Why that order, and why the obvious order is wrong twice

**Sweep the original, then overwrite.** The sweep never reads a value the slots
produced, and the slots never read a value the sweep produced. Both are computed
from the same untouched upstream body, and the strict result wins wherever the
two overlap.

Two failures rule out the alternatives, and both were found on the design rather
than in code:

**Sweeping the already-restored body corrupts a multi-turn session.** The
idempotence argument this spec first made — "a region the slots restored holds no
placeholders, so the sweep does nothing there" — **is false**, and it is false in
exactly the case #32 exists for. If turn one issued `[PERSON_1]` and a later
request legitimately contains that literal, `reserve_literals` cannot map it to
itself because the session already owns the token; `proxy.rs` records this
limitation in those words. Strict slot restoration then inserts the caller's
literal verbatim, and a sweep over the result replaces it with turn one's value.
The client receives a string it never sent.

**Sweeping first and stopping there makes described fields lenient.** An
unmappable token in `content` would be served instead of refusing, which
contradicts this design's own guarantee that nothing which refuses today stops
refusing.

Overwriting is what satisfies both: the sweep may leniently restore `content`,
and the slot's strict restoration of `content` — from the original — replaces it,
refusing if the token cannot be mapped. No value is ever restored twice.

### What stays a slot: every described field, unchanged

**Every field described today keeps its slot and its strict restoration.** The
sweep adds coverage for fields nobody describes; it takes nothing over.

Dropping the slot for ordinary text or a non-embedded document — on the grounds
that the sweep reaches them anyway — would silently move `content` and
`tool_use.input` from strict to lenient, which is the same regression by a
different route.

One case additionally *cannot* be served by the sweep at all, and is why slots
would be needed even if leniency were not a concern: **a string holding a
document**, `tool_calls[].function.arguments`. `restore_value` on a string does a
plain substitution, so a restored value containing a quote, a backslash or a
newline breaks the JSON *text* it is substituted into. `embedded: true` parses,
restores per leaf and re-serializes, and `write_document` escapes correctly.

The error-body path and the streamed path are unchanged. Both already restore
whole and neither gains nor loses anything here.

**But be exact about what the sweep is, because the tempting sentence is false.**
Those two paths restore whole **strictly**: `restore` raises `Unknown` on a token
it cannot map, so an unmappable token refuses there. A lenient sweep is therefore
a **fourth** policy, not the third path falling into line with the other two.

It is worth adding anyway, and the reason is that the two existing whole-body
sites are not comparable to this one. A streamed event and an error envelope
quote what *we sent*, so a placeholder-shaped token in them is overwhelmingly one
we issued, and refusing is right. A success body is the model's own output, where
a token can equally be something the model invented — `[ANYTHING_1]` in a field
nobody described — and `reserve_literals` does not cover that, because it
reserves what went **up** in this request and never saw the model's invention.

Strict here would let a model kill a paid-for answer by writing bracket-shaped
text into a field this gateway does not care about. That is the trade, made
deliberately, and #32 is what removes the need for it.

**The structural win**, stated carefully because an earlier draft overstated it.
A description still decides how strictly a field is treated, and that does not
change. What changes is what a *missing* description costs: today it means the
field is not restored at all, and after this it means the field is restored
leniently. An unknown field degrades to naive-but-correct instead of to a leak —
the same inversion applied four times during the tool-traffic slice, where an
unknown thing had to land in the safe branch rather than the hole.

### Leniency, and exactly where it stops

In the sweep, a token with no mapping is **left in place**. It does not refuse.

The rule about placeholder-shaped **keys** has two halves and they are treated
differently, because one is decidable and the other is not:

- a key that **carries** a token proven to be ours, by lookup, **still refuses**
  — decidable, and it stays strict everywhere;
- a key that merely **is** placeholder-shaped refuses only inside a described
  slot, as today. In the sweep it is left.

## What this deliberately does not close

`restore` cannot tell a token this gateway issued from one that merely looks like
it. So a token that **is** ours but has no mapping — an evicted session — is left
in an undescribed field rather than refused.

**This is strictly better than today**, where undescribed fields are not scanned
at all and the token is forwarded always. It is not complete.

**Issue #32 completes it.** Give the issued token a component the caller could not
have written and the two cases separate exactly: an undescribed field can then
refuse on a token proven ours and leave a stranger's alone. This design's
leniency is the first half of that, not a compromise to revisit. The order — #31
then #32 — is deliberate.

The README's promise is narrowed to match: a placeholder issued by this gateway
does not reach the client **from a field the gateway describes**, and elsewhere
everything with a mapping is restored. #32 is what restores the unqualified
sentence, and the README says so.

## Anthropic's response-side refusals stay

The closed list of response block types and their fields was installed because
the gateway touched only what it described: a block carrying both `text` and
`input` restored neither. The sweep removes that failure, which weakens the
refusals' original justification.

**They stay, unchanged**, for a reason the sweep does not cover: an unknown block
carrying an **embedded** document would be restored naively and corrupted, and
whether it carries one cannot be known without describing it. On well-formed
traffic the refusals cost nothing.

**Be precise about what "not touched" means here.** The sweep lives in `serve`
and is provider-agnostic, so Anthropic's buffered responses gain it too — that is
correct and intended, and it is the same coverage improvement for both. What is
not touched is Anthropic's **refusal lists**, which keep the shape rounds four
and seven gave them.

## Cost

The sweep walks every string in the response rather than two or three. It is a
string scan, not detection, so it is free against a detector round-trip — but it
is no longer constant in the size of the response, and a long completion pays for
it. Stated rather than discovered later.

## Testing

The standard is mutation: break the invariant, run the **named** test, check
*why* it failed, restore.

1. **The two fields from #31** — `refusal` and a citation title reach the client
   restored. Asserted on what the client received, not on what the upstream saw.
2. **Additivity, tested rather than asserted.** A response carrying an unmappable
   placeholder-shaped token in an undescribed field is **served**, not refused.
   "The suite stayed green" only covers what the suite already tests.
3. **Embedded document integrity.** A restored value containing a quote, a
   backslash and a newline inside `arguments` must produce valid JSON. **This is
   also the test that proves the ordering**: mutating the sweep to run first
   fails it, and mutating it to run first also fails a strictness test in a
   described field. The order is held from both sides.
4. **The multi-turn session case**, which is what killed the idempotence
   argument. Turn one issues `[PERSON_1]`; a later request legitimately contains
   that literal in a described field; the client must receive its own literal
   back, not turn one's value. This is the test that fails if the sweep is ever
   moved after the slots, and it is the reason the order is what it is rather
   than a preference.
5. **The decidable key half stays strict** in an undescribed field; the
   undecidable half is left.
6. **Provider parity.** One response shape driven through **both** providers,
   asserting the same treatment.

Item 6 is the only test here that catches the **class** rather than the instance.
The others prove these two fields are fixed; that one exists so the next pair
does not diverge again — which is how this bug, and three like it, were made.

## Out of scope

- Unifying the three restoration policies into one function parameterised by
  strictness. It is the right observation and the wrong moment: it touches the
  streamed path, which this work does not require, and which is the most delicate
  code in the gateway. Recorded as the follow-up.
- Anthropic's response-side refusals, above.
- Issue #32, above.
