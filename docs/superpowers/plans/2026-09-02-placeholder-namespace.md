# Per-Session Placeholder Namespace — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An issued placeholder carries a per-session namespace the caller cannot have written, so telling our token from the caller's stops being a question about shape and becomes a lookup.

**Architecture:** `Mapping` gains a `namespace` derived from the session key — stable across eviction, which minting would not be — and cloned into each request's working copy. The token grammar accepts both `[TYPE_N]` and `[TYPE_N.salt]`; recognising a token and owning it become two questions, and only ownership consults the salt. Ownership then replaces `Provenance`, and the mechanisms that existed to answer ownership by shape are deleted.

**Tech Stack:** Rust (gateway), `getrandom` 0.2 (already a dependency), Python (detector evaluation harness).

## Global Constraints

- **Nothing that refuses today may stop refusing.** New refusals are allowed and are the point; silently serving something previously refused is not.
- **Every invariant is proved by mutation**: break it, run the *named* test, check *why* it failed, restore by **inverse text substitution** — never `git checkout`.
- **Namespace: twelve lowercase hex characters, six bytes.** Copied verbatim from the spec; do not choose a different width. Sixteen bits was the first answer and was wrong — a caller can enumerate 65 536 candidates in about a megabyte of request text, and the caller is who this protects against.
- **The namespace is derived from the session key, not minted per `Mapping`.** Minting fails across eviction, which is the path that produces the tokens Task 6 exists to refuse.
- **No salt may reach a journal line.** The journal writes no token at all today, so this is a property to keep rather than a change to make — see Task 5.
- **`stream::MAX_HELD` becomes 80.** Worst case is `[` + 40 + `_` + 20 digits + `.` + 12 + `]` = 76.
- Local gate for every task: `cargo test --manifest-path gateway/Cargo.toml`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`.

## File Structure

- `gateway/src/mapping.rs` — the namespace, the grammar, ownership, and the deletions. Already large; this slice does not split it, because every change lands on functions that must stay next to each other to be read.
- `gateway/src/stream.rs` — one constant and two comments.
- `gateway/src/proxy.rs` — the request-wide literal walk is deleted; the sweep's call site changes shape.
- `gateway/src/audit.rs` — unchanged in behaviour; one test proves it.
- `evaluation/placeholder_shape.py` — new, Task 1 only.

---

### Task 1: Measure what a salted token does to the detector

**Nothing else in this plan starts until this task's numbers exist.** The spec's fallback applies: if the numbers move outside the gates `evaluation/evaluate.py` already enforces, stop and report rather than adjusting the salt until they come back.

**Files:**
- Create: `evaluation/placeholder_shape.py`

**Interfaces:**
- Consumes: nothing.
- Produces: a recorded measurement, no code other tasks call.

- [ ] **Step 1: Write the measurement script**

```python
"""Measure what a salted placeholder does to detection (#32).

A later turn's text carries placeholders from earlier turns. This asks whether
`[PERSON_1.3f7a2b91c4d5]` changes what the detector finds, against `[PERSON_1]` and
against no token at all.

Run from the repository root:
    uv run --project detector python evaluation/placeholder_shape.py
"""

import json
import sys
from pathlib import Path

from tessera_detector.pipeline import build_detector

CORPUS = Path("evaluation/corpus")
SHAPES = {
    "none": "",
    "bare": "[PERSON_1]",
    "salted": "[PERSON_1.3f7a2b91c4d5]",
}


def documents():
    for path in sorted(CORPUS.glob("*.json")):
        yield path.name, json.loads(path.read_text())["text"]


def run(detector, text):
    return {(s.entity_type, s.start, s.end) for s in detector.detect(text)}


def main():
    detector = build_detector()
    rows = []
    for name, text in documents():
        base = run(detector, text)
        for shape, token in SHAPES.items():
            if not token:
                continue
            # The token goes where a real one would: in front of the text, as
            # an earlier turn's mask echoed back into this one.
            probe = f"{token} {text}"
            found = run(detector, probe)
            shifted = {(t, s - len(token) - 1, e - len(token) - 1) for t, s, e in found}
            inside = [f for f in found if f[1] < len(token)]
            rows.append(
                {
                    "document": name,
                    "shape": shape,
                    "spans_inside_the_token": len(inside),
                    "lost": len(base - shifted),
                    "gained": len(shifted - base),
                }
            )

    totals = {}
    for row in rows:
        acc = totals.setdefault(row["shape"], {"inside": 0, "lost": 0, "gained": 0})
        acc["inside"] += row["spans_inside_the_token"]
        acc["lost"] += row["lost"]
        acc["gained"] += row["gained"]

    print(json.dumps({"per_shape": totals, "documents": len(list(documents()))}, indent=2))

    bare, salted = totals.get("bare"), totals.get("salted")
    if bare is None or salted is None:
        print("both shapes must run", file=sys.stderr)
        return 1
    # The question is whether SALTED is worse than BARE, not whether either is
    # perfect: a bare token in text is what ships today.
    worse = (
        salted["inside"] > bare["inside"]
        or salted["lost"] > bare["lost"]
        or salted["gained"] > bare["gained"]
    )
    print(f"\nsalted worse than bare: {worse}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
```

- [ ] **Step 2: Run it and record the numbers**

Run: `uv run --project detector python evaluation/placeholder_shape.py`

Record the printed JSON verbatim in the task report. Then run the existing gate to confirm nothing else moved:

Run: `uv run --project detector python evaluation/evaluate.py`
Expected: PASS, same as on `main`.

- [ ] **Step 3: Decide, and say which way**

If `salted worse than bare` is `false`, continue to Task 2 and quote the numbers.

If it is `true`, **stop**. Write the numbers into the task report, state which of the three counters moved, and hand back. Do not shorten the salt, change the delimiter, or reshape the token to make this pass — that is fitting the design to the measurement after seeing it, and the spec forbids it by name.

- [ ] **Step 4: Commit**

```bash
git add evaluation/placeholder_shape.py
git commit -m "eval: measure what a salted placeholder does to detection (#32)"
```

---

### Task 2: Derive the namespace and issue tokens into it

**Files:**
- Modify: `gateway/src/mapping.rs` (the `Mapping` struct, `Default`, `placeholder_for`, `placeholder_type`), `gateway/src/session.rs` (construct with `for_session`), `gateway/src/proxy.rs` (the sessionless path keeps `Mapping::new()`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `Mapping::with_namespace(namespace: &str) -> Mapping` — deterministic construction, for tests.
  - `Mapping::namespace(&self) -> &str`
  - `Mapping::for_session(key: &SessionKey) -> Mapping` — the derived namespace.
  - `fn placeholder_namespace(candidate: &str) -> Option<&str>` — the namespace a token carries, `None` for a bare token or a non-token.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn an_issued_token_carries_this_mappings_salt_and_a_callers_literal_does_not() {
    let mut mapping = Mapping::with_namespace("3f7a2b91c4d5");
    let token = mapping
        .mask("Martina Weber", &[span("PERSON", 0, 13)])
        .unwrap();

    assert_eq!(token, "[PERSON_1.3f7a2b91c4d5]", "an issued token lost its namespace");
    assert_eq!(placeholder_namespace(&token), Some("3f7a2b91c4d5"));
    // Recognised, and not ours. Both halves matter: a stranger's token has to
    // be seen in order to be left alone deliberately.
    assert!(is_placeholder("[PERSON_1]"), "a bare token stopped being one");
    assert_eq!(placeholder_namespace("[PERSON_1]"), None);
    assert_eq!(placeholder_type("[PERSON_1.3f7a2b91c4d5]"), Some("PERSON"));
}

#[test]
fn two_mappings_do_not_share_a_namespace() {
    // The salt is minted, not derived: two sessions that mask the same value
    // must not produce the same token, or the namespace is not per-session.
    let mut first = Mapping::new();
    let mut second = Mapping::new();
    let a = first.mask("Martina Weber", &[span("PERSON", 0, 13)]).unwrap();
    let b = second.mask("Martina Weber", &[span("PERSON", 0, 13)]).unwrap();

    assert_ne!(a, b, "two mappings issued the same token");
    assert_eq!(first.namespace().len(), 12);
    assert!(first.salt().chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --manifest-path gateway/Cargo.toml -- an_issued_token_carries two_mappings_do_not_share`
Expected: FAIL — `with_namespace`, `namespace` and `placeholder_namespace` do not exist.

- [ ] **Step 3: Add the salt to the struct**

Remove `Default` from the derive list on `Mapping` and write it, so there is no way to construct a mapping without a namespace:

```rust
/// Bytes of namespace: six, printed as twelve lowercase hex characters.
///
/// **Sixteen bits was the first answer and it was wrong.** The reasoning was
/// that a session is keyed by *(credential, session id)* and a different key
/// reaches nothing — true, and beside the point. What this protects is the
/// integrity of *the caller's own text*, and the caller holds the credential,
/// so the adversary is inside the boundary that argument appealed to. A request
/// can carry all 65 536 four-character candidates in about a megabyte; the one
/// matching a mapping is restored, and the substitution defect is back.
///
/// Forty-eight bits is not enumerable within any request budget, and that is
/// what pays for deleting the request-written provenance set.
const NAMESPACE_BYTES: usize = 6;

#[derive(Debug, Clone)]
pub struct Mapping {
    by_value: HashMap<String, String>,
    by_placeholder: HashMap<String, String>,
    order: Vec<String>,
    next: usize,
    redacted: usize,
    /// The namespace this mapping issues into. Minted once and carried by
    /// every clone, so a request's working copy issues into its session's
    /// namespace and a token from turn one is still ours in turn three.
    namespace: String,
}

impl Default for Mapping {
    fn default() -> Self {
        Self {
            by_value: HashMap::new(),
            by_placeholder: HashMap::new(),
            order: Vec::new(),
            next: 0,
            redacted: 0,
            namespace: mint_namespace(),
        }
    }
}

/// Twelve lowercase hex characters from the operating system's generator, for
/// a mapping with no session behind it.
///
/// **A failure here is fatal on purpose.** The alternative to a random
/// namespace is a predictable one, and a predictable namespace silently
/// restores the defect this exists to close — the gateway would keep running
/// and keep corrupting a caller's own tokens. `session::salt` takes the same
/// line, in the same words, three files away.
fn mint_namespace() -> String {
    let mut bytes = [0u8; NAMESPACE_BYTES];
    getrandom::getrandom(&mut bytes)
        .expect("the operating system's random generator is unavailable");
    hex(&bytes)
}

/// The namespace a session issues into: stable across eviction, because the
/// key is.
///
/// **Minting per mapping fails on exactly the path Task 6 is written for.**
/// Eviction removes a whole session; the next request with the same id builds a
/// fresh `Mapping`, and a minted namespace would differ. A token from the
/// evicted incarnation would then read as a stranger's — left rather than
/// refused, and delivered to the client.
///
/// `SessionKey` is already a salted fingerprint: `session::salt()` is thirty-two
/// random bytes per process mixed into the credential hash, so this is
/// unguessable for the reason the key is, and no new secret is introduced.
pub fn for_session(key: &SessionKey) -> Self {
    let mut hasher = Sha256::new();
    hasher.update(key.fingerprint());
    hasher.update(key.id().as_bytes());
    Self {
        namespace: hex(&hasher.finalize()[..NAMESPACE_BYTES]),
        ..Self::default()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl Mapping {
    /// A mapping issuing into a named namespace. Tests only: production mints.
    pub fn with_namespace(namespace: &str) -> Self {
        Self {
            namespace: namespace.to_owned(),
            ..Self::default()
        }
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }
}
```

- [ ] **Step 4: Issue salted tokens**

In `placeholder_for`, change the minting line:

```rust
        let placeholder = loop {
            self.next += 1;
            let candidate = format!("[{entity_type}_{}.{}]", self.next, self.salt);
```

- [ ] **Step 5: Teach the grammar both forms**

```rust
/// The type name a placeholder carries, or `None` if the candidate is not one.
///
/// **Both forms are placeholders**: `[PERSON_1]`, which only a caller writes,
/// and `[PERSON_1.3f7a2b91c4d5]`, which only this gateway issues. Recognising a token
/// and owning it are different questions, and this one is recognition —
/// `placeholder_namespace` is ownership. A stranger's token has to be recognised
/// precisely so that it can be left alone deliberately rather than by accident.
fn placeholder_type(candidate: &str) -> Option<&str> {
    let inner = candidate
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))?;
    let inner = match inner.rsplit_once('.') {
        Some((before, namespace)) if is_namespace_characters(namespace) => before,
        _ => inner,
    };
    let (entity_type, number) = inner.rsplit_once('_')?;
    let well_formed = is_type_characters(entity_type)
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit());
    well_formed.then_some(entity_type)
}

/// The salt a token carries, or `None` if it carries none.
///
/// Asked of the token and not of the mapping, so that "is this well formed"
/// and "is this ours" stay two questions with one answer each.
pub(crate) fn placeholder_namespace(candidate: &str) -> Option<&str> {
    let inner = candidate
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))?;
    let (before, namespace) = inner.rsplit_once('.')?;
    (is_namespace_characters(namespace) && placeholder_type(&format!("[{before}]")).is_some())
        .then_some(namespace)
}

/// Four lowercase hex characters, which is what `mint_namespace` produces.
///
/// Exact rather than permissive: a token whose suffix is not a salt this
/// gateway could have minted is a token with a dot in it, and reading that dot
/// as a namespace would let a caller name one.
fn is_namespace_characters(namespace: &str) -> bool {
    namespace.len() == NAMESPACE_BYTES * 2
        && namespace
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}
```

- [ ] **Step 5b: Construct session mappings through `for_session`**

`session.rs` builds a mapping in two places — when a session is created and
when one is recreated after eviction. Both become `Mapping::for_session(&key)`,
and the key is already in scope at both. Add:

```rust
    #[test]
    fn a_recreated_session_issues_into_the_same_namespace() {
        // The path that produces the tokens Task 6 refuses. Eviction removes
        // the session; the next request with the same id builds a fresh
        // mapping, and a minted namespace would differ — so a token from the
        // evicted incarnation would read as a stranger's and be delivered
        // rather than refused.
        let key = SessionKey::new(b"k1", "shared");
        let first = Mapping::for_session(&key);
        let second = Mapping::for_session(&key);
        let other = Mapping::for_session(&SessionKey::new(b"k1", "different"));

        assert_eq!(first.namespace(), second.namespace());
        assert_ne!(first.namespace(), other.namespace());
    }
```

Mutation: make `for_session` call `Self::default()` and ignore the key.
Expected: `a_recreated_session_issues_into_the_same_namespace` FAILS on the
first assertion. Restore by inverse text substitution.

- [ ] **Step 6: Run the two new tests**

Run: `cargo test --manifest-path gateway/Cargo.toml -- an_issued_token_carries two_mappings_do_not_share`
Expected: PASS.

- [ ] **Step 7: Move the existing test corpus onto the new token text**

The suite asserts on literal token text in roughly 120 places across
`mapping.rs`, `proxy.rs` and `stream.rs`. They must move, and the move is
mechanical only where the mapping is deterministic.

1. In `mapping.rs` tests, replace `Mapping::new()` with `Mapping::with_namespace("3f7a2b91c4d5")` **only** where the test asserts on token text.
2. Rewrite the asserted literals: `[PERSON_1]` → `[PERSON_1.3f7a2b91c4d5]`, and the same for every type and number.
3. In `proxy.rs` and `stream.rs` tests, the mapping is built inside the router. Add to the test module:

```rust
    /// The token a request's first `PERSON` is masked to, read back from the
    /// body the upstream received rather than assumed — the salt is minted per
    /// session and no test may hard-code it.
    fn issued_token(sent: &str, entity_type: &str, number: u32) -> String {
        let prefix = format!("[{entity_type}_{number}.");
        let start = sent.find(&prefix).expect("no token of that shape was sent");
        let end = sent[start..].find(']').expect("an unterminated token") + start + 1;
        sent[start..end].to_owned()
    }
```

and use it wherever a test previously wrote `"[PERSON_1]"` for a token the
gateway issued. Leave literal `"[PERSON_1]"` untouched wherever the test means
*the caller's own literal* — those are the cases this slice exists for, and
changing them would erase the distinction being built.

- [ ] **Step 8: Run the whole suite**

Run: `cargo test --manifest-path gateway/Cargo.toml`
Expected: PASS. A failure here that is not an assertion on token text is a real
regression — report it rather than adjusting the test.

- [ ] **Step 9: Mutation — the salt is not decoration**

Change `mint_namespace` to return `"000000000000".to_owned()` unconditionally. Run
`two_mappings_do_not_share_a_namespace`. Expected: FAIL, with the two tokens
equal. Restore by inverse text substitution and re-run.

- [ ] **Step 10: Commit**

```bash
git add gateway/src/mapping.rs gateway/src/proxy.rs gateway/src/stream.rs
git commit -m "feat(gateway): issued placeholders carry a per-session salt (#32)"
```

---

### Task 3: Raise the streamed bound

**Files:**
- Modify: `gateway/src/stream.rs:124` and its doc comment; `gateway/src/mapping.rs`'s `MAX_ENTITY_TYPE` comment

**Interfaces:**
- Consumes: `NAMESPACE_BYTES` from Task 2.
- Produces: `stream::MAX_HELD == 80`.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn the_longest_token_this_gateway_can_issue_fits_the_held_bound() {
        // The bound is not a guess: it is what lets the buffer release an
        // unclosed `[` as ordinary text without ever orphaning a real token.
        // Arithmetic, not a sample, so that adding a component to the token
        // fails here rather than in a stream.
        let longest = 1 + crate::mapping::MAX_ENTITY_TYPE + 1 + 20 + 1 + 12 + 1;
        assert_eq!(longest, 76, "the token's worst case moved");
        assert!(
            longest <= MAX_HELD,
            "the longest issuable token no longer fits the held bound"
        );
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --manifest-path gateway/Cargo.toml -- the_longest_token_this_gateway_can_issue`
Expected: FAIL — 76 > 64.

- [ ] **Step 3: Raise the constant and restate both comments**

```rust
/// A `[` that never closes would suspend the stream. Past this many bytes the
/// bracket cannot begin a placeholder, so it is emitted as ordinary text.
///
/// This is a bound on what the masker can issue, not a guess. The worst case is
/// `[` + a name of `mapping::MAX_ENTITY_TYPE` + `_` + twenty digits + `.` + a
/// twelve-character namespace + `]`, which is 76 bytes;
/// `the_longest_token_this_gateway_can_issue_fits_the_held_bound` does the
/// arithmetic so that adding a component to the token fails there rather than
/// orphaning a token in a stream.
pub const MAX_HELD: usize = 80;
```

And in `mapping.rs`, replace the sentence computing 63 with the same arithmetic
reaching 76. **Restate it; do not adjust the number in place** — the next
person to add a component reads that comment, not the plan.

- [ ] **Step 4: Run the suite**

Run: `cargo test --manifest-path gateway/Cargo.toml`
Expected: PASS.

- [ ] **Step 5: Mutation**

Set `MAX_HELD` back to 64. Expected: `the_longest_token_this_gateway_can_issue_fits_the_held_bound` FAILS. Restore by inverse substitution.

- [ ] **Step 6: Commit**

```bash
git add gateway/src/stream.rs gateway/src/mapping.rs
git commit -m "fix(gateway): the held bound had one byte of headroom and the salt needs five (#32)"
```

---

### Task 4: Ownership replaces provenance

**Files:**
- Modify: `gateway/src/mapping.rs` (`Provenance`, `Lenient`, `restore_sweep`, `restore_in_string`), `gateway/src/proxy.rs` (the sweep's call site and the request-wide literal walk)

**Interfaces:**
- Consumes: `placeholder_namespace`, `Mapping::namespace` from Task 2.
- Produces: `Mapping::owns(&self, candidate: &str) -> bool`; `restore_sweep(&self, value: &Value) -> Value` (the `Provenance` parameter is gone); `restore_in_string(&self, text: &str) -> String`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_callers_own_literal_survives_a_turn_that_issued_the_same_bare_token() {
    // The defect, at the unit. Turn one issues for `Martina Weber`; turn two
    // carries the caller's own `[PERSON_1]`. Nothing may rewrite it.
    let mut mapping = Mapping::with_namespace("3f7a2b91c4d5");
    mapping.mask("Martina Weber", &[span("PERSON", 0, 13)]).unwrap();

    assert_eq!(
        mapping.restore_in_string("siehe [PERSON_1] im Muster"),
        "siehe [PERSON_1] im Muster",
        "the caller's own literal was rewritten to a value they never sent"
    );
}

#[test]
fn another_sessions_token_is_left_alone() {
    // Ownership is not "well formed and salted". A token from a different
    // namespace is a stranger's, whatever it looks like.
    let mut mapping = Mapping::with_namespace("3f7a2b91c4d5");
    mapping.mask("Martina Weber", &[span("PERSON", 0, 13)]).unwrap();

    assert!(!mapping.owns("[PERSON_1.0a1b7c33e2f1]"));
    assert_eq!(
        mapping.restore_in_string("[PERSON_1.0a1b7c33e2f1]"),
        "[PERSON_1.0a1b7c33e2f1]",
        "a token from another session was restored"
    );
}

#[test]
fn a_token_this_turn_did_not_issue_still_restores() {
    // The coverage the salt buys. Turn one issues; turn three neither sends
    // nor re-masks the value, and the model echoes the token anyway. Today
    // that comes back as a placeholder because the request's `issued` set does
    // not contain it.
    let mut session = Mapping::with_namespace("3f7a2b91c4d5");
    let token = session
        .mask("Martina Weber", &[span("PERSON", 0, 13)])
        .unwrap();

    let later = session.clone();
    assert_eq!(
        later.restore_in_string(&format!("und {token} dazu")),
        "und Martina Weber dazu",
        "a token from an earlier turn was not restored"
    );
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --manifest-path gateway/Cargo.toml -- a_callers_own_literal_survives another_sessions_token a_token_this_turn_did_not_issue`
Expected: FAIL — `owns` does not exist and `restore_in_string` still takes a `Provenance`.

- [ ] **Step 3: Add ownership**

```rust
    /// Whether this mapping issued `candidate`.
    ///
    /// **The whole of the question, and it is a lookup rather than a shape.**
    /// A token carrying this mapping's salt was minted by it — a caller cannot
    /// have written the salt — so the table settles the rest. `Provenance`
    /// existed because a session outlives a request and a lookup was therefore
    /// not proof; the salt makes it proof, and both of its sets had nothing
    /// left to decide.
    pub fn owns(&self, candidate: &str) -> bool {
        placeholder_namespace(candidate) == Some(self.namespace.as_str())
            && self.by_placeholder.contains_key(candidate)
    }
```

- [ ] **Step 4: Delete `Provenance` and rewire its callers**

- Delete `pub struct Provenance`, `impl Provenance`, and every `use` of it.
- `Lenient` becomes a unit struct; its `token` arm becomes:

```rust
    fn token<'a>(&self, mapping: &'a Mapping, candidate: &'a str) -> Result<&'a str, Infallible> {
        Ok(match mapping.by_placeholder.get(candidate) {
            Some(value) if mapping.owns(candidate) => value,
            // A stranger's token, or one this namespace no longer maps. The
            // token is the answer and `Infallible` says the sweep has no
            // other one.
            _ => candidate,
        })
    }
```

- `restore_sweep` and `restore_in_string` lose their `&Provenance` parameter.
- In `proxy.rs`, delete the request-wide literal walk that built `written` and
  the `Provenance::new(...)` construction, and call `restore_sweep(&upstream)`.

- [ ] **Step 5: Delete the mechanisms that answered ownership by shape**

- `Mapping::reserve_literals` and its call in `mask`.
- The skip-taken-numbers loop in `placeholder_for` becomes a plain increment:

```rust
        self.next += 1;
        let placeholder = format!("[{entity_type}_{}.{}]", self.next, self.salt);
```

- `proxy::mask_all`'s reservation pass over every slot.

**`key_is_unserveable`'s first arm stays**, and was on this list until review.
It refuses *any* exact placeholder-shaped property key; the containment arm that
would remain refuses only keys this mapping maps. Deleting it moves a client
from a refusal to a model-chosen property name — a response that refuses today
ceasing to refuse, which the first global constraint forbids. Leave it, and
leave its test alone.

For each deletion, run the full suite. A test that fails **only** because it
asserts the deleted mechanism exists is rewritten to assert the behaviour that
replaced it. A test that fails for any other reason is a real regression: stop
and report it.

- [ ] **Step 6: Run the new tests, then the suite**

Run: `cargo test --manifest-path gateway/Cargo.toml`
Expected: PASS.

- [ ] **Step 7: Mutations — three, one per new test**

1. Make `owns` ignore the salt (`self.by_placeholder.contains_key(candidate)` alone). Expected: `a_callers_own_literal_survives_a_turn_that_issued_the_same_bare_token` FAILS with the literal rewritten to `Martina Weber`.
2. Make `owns` ignore the table (`placeholder_namespace(candidate) == Some(self.namespace.as_str())` alone). Expected: a test in the existing suite covering an unmapped token FAILS; if none does, add one asserting `!mapping.owns("[PERSON_9.3f7a2b91c4d5]")`.
3. Make `owns` compare only the namespace's *presence* (`placeholder_namespace(candidate).is_some()`). Expected: `another_sessions_token_is_left_alone` FAILS.

Restore each by inverse text substitution and re-run.

- [ ] **Step 8: Commit**

```bash
git add gateway/src/mapping.rs gateway/src/proxy.rs
git commit -m "feat(gateway): ownership is a lookup, so provenance and four shape mechanisms go (#32)"
```

---

### Task 5: Prove the salt cannot reach a journal line

**The spec decided the journal records the unsalted token. Checking the code
before writing any turned that decision into a free one:** `audit.rs`'s module
comment says *"Nothing here ever writes a submitted value, a hash of one, an
offset or a placeholder name. What it writes is counts, a fixed vocabulary of
error classes, and salted digests"*. The journal has never carried a token, so
there is nothing to strip.

That same comment records having been false once, for the one field whose keys
come from outside the perimeter. **So this task pins the property rather than
implementing it**, and no `unsalted` helper is written — an unused helper reads
as coverage, which this repository treats as worse than absent code.

**Files:**
- Modify: `gateway/src/audit.rs` (test module only)

**Interfaces:**
- Consumes: `Mapping::with_namespace` from Task 2.
- Produces: nothing. No production code changes in this task.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn no_journal_line_can_carry_a_placeholder_or_its_namespace() {
        // The module comment claims this and has been wrong about it once,
        // through `types` — the one field whose keys come from outside. A
        // per-session salt in a line would make two sessions incomparable and
        // put a per-session value in a log, so the claim is pinned here rather
        // than trusted.
        let mut mapping = crate::mapping::Mapping::with_namespace("3f7a2b91c4d5");
        let token = mapping
            .mask("Martina Weber", &[span("PERSON", 0, 13)])
            .unwrap();

        let line = a_line_recording(&token);

        assert!(!line.contains(&token), "a journal line carried a token: {line}");
        assert!(!line.contains("3f7a2b91c4d5"), "a journal line carried a namespace: {line}");
        assert!(!line.contains("PERSON_1"), "a journal line carried a token body: {line}");
    }
```

`a_line_recording` is the existing test helper that drives a `Record` to a
written line; find it in `audit.rs`'s test module and use the one already there
rather than adding another. If a detector type reaches the line, that is the
`types` field and it is a type name — `PERSON` on its own is expected and is why
the assertion is on `PERSON_1`.

- [ ] **Step 2: Run it**

Run: `cargo test --manifest-path gateway/Cargo.toml -- no_journal_line_can_carry_a_placeholder`
Expected: PASS on the first run. **A test that passes immediately is only
evidence once it has been mutated** — see the next step.

- [ ] **Step 3: Mutation**

Add the token to a journal line deliberately: in whatever `Record` field the
helper writes, push `token` into the `types` map as a key. Expected: the test
FAILS on the `PERSON_1` assertion. Restore by inverse text substitution and
re-run.

- [ ] **Step 4: Commit**

```bash
git add gateway/src/audit.rs
git commit -m "test(gateway): pin that no journal line can carry a token or its namespace (#32)"
```

---

### Task 6: Spend the certainty

**Files:**
- Modify: `gateway/src/mapping.rs` (`Lenient::token`), `README.md`, `docs/frontend-handoff.md`

**Interfaces:**
- Consumes: `Mapping::owns` from Task 4.
- Produces: no new API. The lenient sweep gains a refusal it did not have.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn a_token_this_namespace_issued_but_cannot_map_refuses_rather_than_reaching_the_client() {
    // #31's residual, and the promise this slice restores. Ours by salt, with
    // no mapping — an evicted session, a stale turn. Leaving it puts a token
    // this gateway issued in front of the client, which is the one thing the
    // first clause promises it will not.
    let mapping = Mapping::with_namespace("3f7a2b91c4d5");

    assert!(
        matches!(
            mapping.restore_sweep(&json!({"note": "[PERSON_9.3f7a2b91c4d5]"})),
            Err(MappingError::Unknown(_))
        ),
        "a token this namespace issued was served to the client"
    );
    // A stranger's is still left: refusing on one would refuse on a caller's
    // own text, which is the defect this slice closed.
    assert_eq!(
        mapping
            .restore_sweep(&json!({"note": "[PERSON_9.0a1b7c33e2f1]"}))
            .expect("a stranger's token was refused"),
        json!({"note": "[PERSON_9.0a1b7c33e2f1]"})
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --manifest-path gateway/Cargo.toml -- a_token_this_namespace_issued_but_cannot_map`
Expected: FAIL — `restore_sweep` returns `Value`, not `Result`, so the test does
not compile. That is the signature this task changes.

- [ ] **Step 3: Give the sweep a refusal for tokens it owns by salt alone**

The distinction the salt now makes available: **carries our salt** (ours,
whatever the table says) versus **carries our salt and is mapped** (`owns`).
Add the first as a private predicate and refuse on ours-but-unmapped:

```rust
    /// Whether this token was minted by this mapping's namespace, mapped or
    /// not. `owns` is this plus a mapping; the difference is exactly the case
    /// #31 had to leave and this slice can refuse.
    fn minted_here(&self, candidate: &str) -> bool {
        placeholder_namespace(candidate) == Some(self.namespace.as_str())
    }
```

and in the sweep's token arm, before falling through to leaving the token:

```rust
            _ if mapping.minted_here(candidate) => {
                return Err(MappingError::Unknown(candidate.to_owned()))
            }
```

which requires the sweep's rule to stop being `Infallible`. Change `Lenient`'s
`Error` to `MappingError` and let the two call sites propagate. **This is the
one behaviour change in the slice that can refuse a response that succeeds
today**, and it is the point: those responses were carrying a placeholder.

- [ ] **Step 4: Run the suite**

Run: `cargo test --manifest-path gateway/Cargo.toml`
Expected: PASS. Any test that asserted a placeholder reaching the client from an
undescribed field now asserts a refusal; rewrite it and say so in the report.

- [ ] **Step 5: Restore the promise in the documents**

In `README.md` and `docs/frontend-handoff.md`, the downward guarantee currently
reads *a placeholder issued by the gateway does not reach the client from a
field the gateway describes*. Remove the qualification — it is now unqualified —
and delete the sentence naming #32 as what would restore it. **Replace the
claim; do not annotate it.** The rule this repository learned the hard way is
that a reader implements the definition, not the note beneath it.

- [ ] **Step 6: Mutation**

Remove the `minted_here` arm from the sweep. Expected:
`a_token_this_namespace_issued_but_cannot_map_refuses_rather_than_reaching_the_client`
FAILS. Restore by inverse substitution.

- [ ] **Step 7: Commit**

```bash
git add gateway/src/mapping.rs README.md docs/frontend-handoff.md
git commit -m "feat(gateway): a placeholder this gateway issued no longer reaches the client (#32)"
```

---

## Self-review

**Spec coverage.** Salt shape and length → Task 2. Grammar accepts both forms →
Task 2. Streamed bound → Task 3. `Provenance` collapse and the deletions →
Task 4. Journal → Task 5, where checking the code first turned a decision into
a property to pin: the journal has never carried a token, so there is nothing to
strip and no helper to write. #31's residual and the README promise → Task 6.
Measurement first with a stop-and-report fallback → Task 1. Every section has a
task.

**Placeholders.** None: every code step carries the code.

**Type consistency.** `placeholder_namespace` returns `Option<&str>` and is used that
way in Tasks 2, 4, 5 and 6. `owns` and `minted_here` are distinct by design and
their difference is the subject of Task 6's test. `with_salt` is used in every
deterministic test and never in production.

**One risk the plan carries deliberately.** Task 2's step 7 moves roughly 120
assertions. It is mechanical, it is the largest single edit in the slice, and it
is the place a real regression is most likely to be mistaken for churn — hence
the instruction to report any failure that is not an assertion on token text
rather than adjusting it.
