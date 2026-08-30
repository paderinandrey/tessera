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

**Restore the slots first, exactly as today. Then sweep the whole body
leniently.**

The change is **purely additive**. Nothing that refuses today stops refusing;
nothing that succeeds today starts failing. Coverage grows and nothing else
moves.

### Why that order

Strictness differs by field and a whole-body walk cannot tell which field it is
in. Sweeping first would make `content` lenient too — an unknown placeholder in a
described field would stop refusing, which is a regression nobody asked for.

Slots first keeps every existing guarantee in place, and the sweep then reaches
what the slots did not.

### Why no skip list is needed

Restoration is idempotent. A region the slots already restored holds no
placeholders, so the sweep does nothing there. This matters because the
alternative — sweeping everything *except* the slot pointers — needs a list of
what to skip, and a hand-written list of what matters has been wrong five times
in this codebase. Idempotence does the work a list would have done badly.

Two cases make the idempotence claim non-obvious and both hold:

- an embedded `arguments` document, after slot restoration, is re-serialized
  with real values and carries no token for the sweep to find;
- a restored value that legitimately contains a bracket-shaped token is mapped to
  itself by `reserve_literals`, so the sweep restores it to itself.

### What stays a slot

**Exactly one case: a string holding a document** — `tool_calls[].function.arguments`
on OpenAI, and nothing else today.

`restore_value` on a string does a plain substitution. If a restored value
contains a quote, a backslash or a newline, substituting it into JSON *text*
breaks the syntax. The request path already solves this with `embedded: true` —
parse, restore per leaf, re-serialize — and `write_document` escapes correctly.
So the slot is a requirement here, not a preference.

Ordinary text and a non-embedded document need no slot on the response path: they
are nested values and the sweep handles them.

The error-body path and the streamed path are unchanged. Both already restore
whole and neither gains or loses anything here; this brings the third path into
line with them rather than inventing a fourth policy.

**The structural win.** Whether a field is described stops deciding *whether* it
is handled and starts deciding only *how*. An undescribed field degrades to naive
but correct restoration instead of to a leak. That is the same inversion applied
four times during the tool-traffic slice: an unknown thing must land in the safe
branch, not in the hole.

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
4. **Idempotence**, proved rather than argued: a restored value legitimately
   containing a bracket-shaped token survives the sweep unchanged.
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
