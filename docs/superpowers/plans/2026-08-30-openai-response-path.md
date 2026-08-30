# Closing OpenAI's Response Path — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A placeholder this gateway issued stops reaching the client from response fields nobody described, without any request that succeeds today starting to fail.

**Architecture:** `serve` sweeps the untouched upstream body with a lenient, provenance-gated restoration, then overwrites each described field with its existing strict slot restoration computed from that same untouched body. Neither pass reads the other's output.

**Tech Stack:** Rust, `serde_json`, `axum`, existing `Mapping`/`Slot`/`provider` seams. No new dependencies.

## Global Constraints

- **Spec:** `docs/superpowers/specs/2026-08-30-openai-response-path-design.md`. Read it before Task 1; every task below implements part of it.
- **Additivity is the invariant of the whole plan.** Nothing that refuses today may stop refusing, and nothing that succeeds today may start failing. Any task that cannot hold this has found a design problem — report it rather than trading it away.
- **The sweep never refuses.** Its restoration type is infallible. Strictness lives only in the existing slot path.
- **Mutation standard:** break the invariant, run the **named** test, check *why* it failed, restore by inverse text substitution, record the output verbatim. Never `git checkout <file>` to restore a mutation — this repo lost a fix that way.
- **Verification before any commit:** `cd gateway && cargo test --quiet && cargo fmt --check && touch src/*.rs && cargo clippy --all-targets -- -D warnings`
- **Baseline:** 488 tests on `main`. Report the count after every task.
- Commit signing is flaky here (1Password agent). If signing fails, leave the work plus the message and say so; do not work around it.

---

### Task 1: `Provenance` — the two sets and the rule between them

**Files:**
- Modify: `gateway/src/mapping.rs`

**Interfaces:**
- Produces: `pub struct Provenance`, `Provenance::new(issued: HashSet<String>, written: HashSet<String>) -> Provenance`, `Provenance::restorable(&self, token: &str) -> bool`. Tasks 2, 3, 5 and 6 all use these names.

- [ ] **Step 1: Write the failing test**

In `mapping.rs`'s `mod tests`:

```rust
    #[test]
    fn a_token_is_restorable_only_if_this_request_issued_it_and_the_caller_did_not_write_it() {
        let issued = HashSet::from(["[PERSON_1]".to_owned(), "[IBAN_2]".to_owned()]);
        let written = HashSet::from(["[PERSON_1]".to_owned(), "[ORG_9]".to_owned()]);
        let provenance = Provenance::new(issued, written);

        // Issued and not written: the ordinary case, and the whole point.
        assert!(provenance.restorable("[IBAN_2]"));
        // Issued *and* written: the two occurrences are the same bytes in the
        // response, so neither can be told from the other. Left.
        assert!(!provenance.restorable("[PERSON_1]"));
        // Written only: the caller's own literal.
        assert!(!provenance.restorable("[ORG_9]"));
        // Neither: the model invented it.
        assert!(!provenance.restorable("[PERSON_7]"));
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd gateway && cargo test --quiet a_token_is_restorable_only_if`
Expected: FAIL to compile — `Provenance` does not exist.

- [ ] **Step 3: Implement**

In `mapping.rs`, beside `Mapping`:

```rust
/// Which tokens in a response this gateway may claim as its own.
///
/// A `by_placeholder` lookup is **not** provenance: a session outlives a
/// request, so a token turn one issued is in the table when turn three's caller
/// writes that same literal themselves. Restoring on the lookup would hand the
/// client turn one's value in place of its own text, and refusing on it would
/// reject a response that is served today. Both were tried; the spec records
/// them.
///
/// So provenance is built from this request and nothing else.
pub struct Provenance {
    /// Tokens `placeholder_for` returned during this request's mask pass. A
    /// caller's literal never reaches `placeholder_for` — `reserve_literals` is
    /// the only thing that sees one — so this set cannot be forged from the
    /// request body.
    issued: HashSet<String>,
    /// Placeholder-shaped tokens the request body carried, from **every**
    /// string in it. Not from `reserve_literals`: that runs only inside
    /// provider-selected slots, and dispatch strings are deliberately not
    /// slots, so a tool name `lookup_[PERSON_1]` would be invisible to it and
    /// the echoed name would come back as `lookup_Martina Weber` — a broken
    /// call the client cannot diagnose.
    written: HashSet<String>,
}

impl Provenance {
    pub fn new(issued: HashSet<String>, written: HashSet<String>) -> Self {
        Self { issued, written }
    }

    /// Whether the sweep may restore this token. A token in both sets is
    /// ambiguous by construction: the two occurrences reach the response as the
    /// same bytes, and nothing distinguishes them. Left, which loses coverage
    /// and corrupts nothing. #32 is what separates them.
    pub fn restorable(&self, token: &str) -> bool {
        self.issued.contains(token) && !self.written.contains(token)
    }
}
```

- [ ] **Step 4: Run it and watch it pass**

Run: `cd gateway && cargo test --quiet a_token_is_restorable_only_if`
Expected: PASS.

- [ ] **Step 5: Prove it by mutation**

Change `restorable` to `self.issued.contains(token)`. Expected: the named test fails on the `[PERSON_1]` assertion — the overlap case. Restore by inverse text substitution and re-run.

- [ ] **Step 6: Full verification and commit**

```bash
cd gateway && cargo test --quiet && cargo fmt --check && touch src/*.rs && cargo clippy --all-targets -- -D warnings
git add gateway/src/mapping.rs
git commit -m "feat(gateway): provenance is what this request issued, not what the table knows

A by_placeholder lookup is not provenance: a session outlives a request, so
a token turn one issued is in the table when turn three's caller writes that
literal themselves. Restoring on the lookup hands back turn one's value;
refusing on it rejects a response served today."
```

---

### Task 2: `placeholder_literals` — the whole-request walk

**Files:**
- Modify: `gateway/src/mapping.rs`

**Interfaces:**
- Consumes: `pieces`, `Piece` (private to `mapping.rs`).
- Produces: `pub fn placeholder_literals(value: &Value) -> HashSet<String>`. Task 6 calls it.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn the_literal_walk_reads_every_string_including_the_ones_no_slot_addresses() {
        let body = json!({
            "model": "gpt",
            // A dispatch string. `reserve_literals` never sees one, because
            // dispatch is deliberately not a slot — this is the case that
            // would echo back as a broken tool name.
            "tools": [{"type": "function", "function": {"name": "lookup_[PERSON_1]"}}],
            "messages": [
                {"role": "user", "content": "see [ORG_2]"},
                // Nested, and in key position.
                {"role": "user", "content": {"[IBAN_3]": ["deep [PERSON_4]"]}}
            ],
            // Not placeholder-shaped: no type, no number.
            "metadata": {"note": "[not a token]"}
        });

        assert_eq!(
            placeholder_literals(&body),
            HashSet::from([
                "[PERSON_1]".to_owned(),
                "[ORG_2]".to_owned(),
                "[IBAN_3]".to_owned(),
                "[PERSON_4]".to_owned(),
            ])
        );
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd gateway && cargo test --quiet the_literal_walk_reads_every_string`
Expected: FAIL to compile — `placeholder_literals` does not exist.

- [ ] **Step 3: Implement**

```rust
/// Every placeholder-shaped token the request body carries, from every string
/// in it — values, keys, and fields no slot addresses.
///
/// It looks for a lexical shape rather than for meaning, so it needs no
/// provider knowledge and **nothing may be exempt from it**. Exempting a field
/// is how `lookup_[PERSON_1]` in a tool name — dispatch, and deliberately not a
/// slot — would have been missed, and the echoed name restored to
/// `lookup_Martina Weber`.
pub fn placeholder_literals(value: &Value) -> HashSet<String> {
    let mut found = HashSet::new();
    collect_literals(value, &mut found);
    found
}

fn collect_literals(value: &Value, found: &mut HashSet<String>) {
    match value {
        Value::String(text) => {
            for piece in pieces(text) {
                if let Piece::Placeholder(candidate) = piece {
                    found.insert(candidate.to_owned());
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_literals(item, found);
            }
        }
        Value::Object(fields) => {
            for (key, item) in fields {
                // Keys as well as values: a property name is a string the
                // caller chose, and it reaches the response the same way.
                for piece in pieces(key) {
                    if let Piece::Placeholder(candidate) = piece {
                        found.insert(candidate.to_owned());
                    }
                }
                collect_literals(item, found);
            }
        }
        _ => {}
    }
}
```

- [ ] **Step 4: Run it and watch it pass**

Run: `cd gateway && cargo test --quiet the_literal_walk_reads_every_string`
Expected: PASS.

- [ ] **Step 5: Prove it by mutation**

1. Delete the key loop in the `Value::Object` arm. Expected: the named test fails, missing `[IBAN_3]`.
2. Delete the `Value::Array` arm's recursion. Expected: the named test fails, missing `[PERSON_1]` and `[PERSON_4]`.

Restore each by inverse text substitution.

- [ ] **Step 6: Full verification and commit**

```bash
cd gateway && cargo test --quiet && cargo fmt --check && touch src/*.rs && cargo clippy --all-targets -- -D warnings
git add gateway/src/mapping.rs
git commit -m "feat(gateway): the literal walk reads every string in the request

reserve_literals runs only inside provider-selected slots, and dispatch
strings are deliberately not slots — so a tool name lookup_[PERSON_1] is
invisible to it, and the echoed name would come back as
lookup_Martina Weber. This looks for a lexical shape, so nothing is exempt."
```

---

### Task 3: `Mapping` records what this request issued

**Files:**
- Modify: `gateway/src/mapping.rs`

**Interfaces:**
- Produces: `Mapping::begin_request(&mut self)`, `Mapping::issued(&self) -> HashSet<String>`. Task 6 calls both.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_mapping_records_the_tokens_this_request_issued_and_forgets_the_last_ones() {
        let mut mapping = Mapping::default();
        mapping.begin_request();
        let masked = mapping
            .mask("Martina Weber", &[span("PERSON", 0, 13)])
            .unwrap();
        assert_eq!(mapping.issued(), HashSet::from([masked.clone()]));

        // A literal the caller wrote is not issued. `reserve_literals` sees it;
        // `placeholder_for` never does, which is what makes the set unforgeable.
        mapping.reserve_literals("the caller wrote [ORG_5] here");
        assert_eq!(mapping.issued(), HashSet::from([masked.clone()]));

        // A second turn re-masking the same value still issues it: the token is
        // reused from `by_value`, and reuse is issuance for this purpose.
        mapping.begin_request();
        assert_eq!(mapping.issued(), HashSet::new());
        mapping
            .mask("Martina Weber", &[span("PERSON", 0, 13)])
            .unwrap();
        assert_eq!(mapping.issued(), HashSet::from([masked]));
    }
```

If `span(...)` is not the existing helper's name, read a neighbouring `mapping` test and use whatever it actually calls.

- [ ] **Step 2: Run it and watch it fail**

Run: `cd gateway && cargo test --quiet a_mapping_records_the_tokens_this_request_issued`
Expected: FAIL to compile — `begin_request` and `issued` do not exist.

- [ ] **Step 3: Implement**

Add the field to `Mapping`:

```rust
    /// Tokens `placeholder_for` returned since `begin_request`. Per request,
    /// not per session: a session's table is what provenance cannot be read
    /// from. `absorb` does not carry it, for the same reason reserved literals
    /// are absent from `order`.
    issued: HashSet<String>,
```

And the two methods:

```rust
    /// Starts a request's issuance record. `handle` masks into a clone of the
    /// session's mapping, and the clone carries the previous request's set, so
    /// this clears it rather than relying on the clone being fresh.
    pub fn begin_request(&mut self) {
        self.issued.clear();
    }

    pub fn issued(&self) -> HashSet<String> {
        self.issued.clone()
    }
```

In `placeholder_for`, record on **both** return paths — the `by_value` hit and the fresh allocation. Reuse is issuance: the token went up standing for a value this request masked, which is the only property the sweep needs.

- [ ] **Step 4: Run it and watch it pass**

Run: `cd gateway && cargo test --quiet a_mapping_records_the_tokens_this_request_issued`
Expected: PASS.

- [ ] **Step 5: Prove it by mutation**

1. Record only on the fresh-allocation path, not the `by_value` hit. Expected: the named test fails on the second-turn assertion — the reused token is missing.
2. Delete the `clear()` in `begin_request`. Expected: the named test fails on `assert_eq!(mapping.issued(), HashSet::new())`.

- [ ] **Step 6: Confirm `absorb` does not carry it**

Read `absorb` and confirm the issued set does not reach the session. If it does, that is a bug this task introduces — fix it and say so in the report. Add an assertion to the existing session test that pins it.

- [ ] **Step 7: Full verification and commit**

```bash
cd gateway && cargo test --quiet && cargo fmt --check && touch src/*.rs && cargo clippy --all-targets -- -D warnings
git add gateway/src/mapping.rs
git commit -m "feat(gateway): a mapping records what this request issued

Per request rather than per session, because a session's table is exactly
what provenance cannot be read from. Reuse from by_value counts as issuance:
the token went up standing for a value this request masked, which is the
only property the sweep needs."
```

---

### Task 4: JSON-safe substitution inside one string

**Files:**
- Modify: `gateway/src/mapping.rs`

**Interfaces:**
- Consumes: `Provenance` (Task 1), `pieces`/`Piece`.
- Produces: `impl Mapping { pub fn restore_in_string(&self, text: &str, provenance: &Provenance) -> String }`. Task 5 calls it.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn a_value_needing_no_escaping_is_substituted_in_place_and_keeps_formatting() {
        let mut mapping = Mapping::default();
        mapping.begin_request();
        let token = mapping
            .mask("Martina Weber", &[span("PERSON", 0, 13)])
            .unwrap();
        let provenance = Provenance::new(mapping.issued(), HashSet::new());

        // A document with formatting the client may be comparing byte for byte.
        let document = format!("{{\"name\": \"{token}\",  \"ok\": 1}}");
        assert_eq!(
            mapping.restore_in_string(&document, &provenance),
            "{\"name\": \"Martina Weber\",  \"ok\": 1}",
            "a value with no quote, backslash or control character cannot close \
             a string, so the substitution stands and the spacing survives"
        );
    }

    #[test]
    fn a_value_needing_escaping_cannot_inject_fields_into_a_document() {
        let mut mapping = Mapping::default();
        mapping.begin_request();
        // The injection: a value carrying a quote and a comma.
        let hostile = "x\",\"admin\":true,\"unused\":\"y";
        let token = mapping
            .mask(hostile, &[span("PERSON", 0, hostile.chars().count())])
            .unwrap();
        let provenance = Provenance::new(mapping.issued(), HashSet::new());

        let restored = mapping.restore_in_string(&format!("{{\"name\":\"{token}\"}}"), &provenance);
        let parsed: Value = serde_json::from_str(&restored).expect("still a document");
        assert_eq!(
            parsed,
            json!({"name": hostile}),
            "one field carrying the value, not three: the assertion is the \
             injection, not the parse — a corrupted document parses"
        );
    }

    #[test]
    fn a_serialized_scalar_is_a_document_too() {
        let mut mapping = Mapping::default();
        mapping.begin_request();
        let hostile = "Martina \"Weber\"";
        let token = mapping
            .mask(hostile, &[span("PERSON", 0, hostile.chars().count())])
            .unwrap();
        let provenance = Provenance::new(mapping.issued(), HashSet::new());

        let restored = mapping.restore_in_string(&format!("\"{token}\""), &provenance);
        assert_eq!(
            serde_json::from_str::<Value>(&restored).expect("still a document"),
            json!(hostile)
        );
    }

    #[test]
    fn a_token_this_request_did_not_issue_is_left_where_it_stands() {
        let mut mapping = Mapping::default();
        mapping.begin_request();
        mapping.mask("Martina Weber", &[span("PERSON", 0, 13)]).unwrap();
        // Issued, and also written by the caller: ambiguous, so left.
        let provenance = Provenance::new(
            mapping.issued(),
            HashSet::from(["[PERSON_1]".to_owned()]),
        );
        assert_eq!(
            mapping.restore_in_string("see [PERSON_1] and [ORG_9]", &provenance),
            "see [PERSON_1] and [ORG_9]"
        );
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cd gateway && cargo test --quiet restore_in_string a_value_needing a_serialized_scalar a_token_this_request_did_not`
Expected: FAIL to compile — `restore_in_string` does not exist.

- [ ] **Step 3: Implement**

```rust
    /// Restore inside one string, leniently and without breaking a document the
    /// string may be.
    ///
    /// **Lenient**: it never fails. A token this request did not issue, or one
    /// the caller also wrote, is left exactly as it stands — see `Provenance`.
    ///
    /// **The rule is about the inserted value first and the string's shape
    /// second, and in that order it is exact.** Substituting into JSON *text*
    /// is byte-safe precisely when the value carries no `"`, no `\` and no
    /// control character: without a quote the string cannot be closed, and a
    /// comma or a brace inside a JSON string is an ordinary character. So a
    /// value needing no escaping is substituted whatever it sits in, and the
    /// document's formatting survives byte for byte.
    ///
    /// A value that *does* need escaping, into a string that parses as JSON,
    /// forces parse-and-re-serialize. Reformatting is the price of not
    /// corrupting and is paid only there. Into a string that is not JSON there
    /// is no structure to break.
    ///
    /// Testing that the result still *parses* is not enough and was tried:
    /// restoring a token in `{"name":"[PERSON_1]"}` to `x","admin":true,...`
    /// yields **valid** JSON carrying fields nobody sent, which a client's tool
    /// then acts on.
    pub fn restore_in_string(&self, text: &str, provenance: &Provenance) -> String {
        let mut out = String::with_capacity(text.len());
        let mut needs_escaping = false;
        for piece in pieces(text) {
            match piece {
                Piece::Text(run) => out.push_str(run),
                Piece::Placeholder(candidate) => match self.by_placeholder.get(candidate) {
                    Some(value) if provenance.restorable(candidate) => {
                        needs_escaping |= value.chars().any(json_string_unsafe);
                        out.push_str(value);
                    }
                    _ => out.push_str(candidate),
                },
            }
        }
        if !needs_escaping {
            return out;
        }
        // The substitution inserted a character that can close a string. If the
        // original was a document, redo it structurally so the value lands in a
        // leaf and is escaped on the way out.
        // `restore_document` returns None when a restored key would collide
        // with one already in the map; the whole string is then left as it
        // came, because losing a field is worse than losing a restoration.
        match serde_json::from_str::<Value>(text) {
            Ok(document) => match self.restore_document(&document, provenance) {
                Some(restored) => serde_json::to_string(&restored).unwrap_or(out),
                None => text.to_owned(),
            },
            Err(_) => out,
        }
    }

    /// The recursion, which fixes escaping and **does not extend the key rule**.
    ///
    /// Parsing a nested serialized document promotes strings into key positions
    /// that were plain text a moment earlier. Refusing there would reject a
    /// response served today — `{"[PERSON_1]":"ok"}` inside a described
    /// `arguments` is text to `restore_value` now, is substituted, and is
    /// served. Additivity decides it: the key is restored exactly as today's
    /// substitution restores it, and the key rule keeps the depth it has.
    fn restore_document(&self, value: &Value, provenance: &Provenance) -> Option<Value> {
        Some(match value {
            Value::String(text) => Value::String(self.restore_in_string(text, provenance)),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| self.restore_document(item, provenance))
                    .collect::<Option<Vec<_>>>()?,
            ),
            Value::Object(fields) => {
                let mut out = serde_json::Map::with_capacity(fields.len());
                for (key, item) in fields {
                    let restored = self.restore_in_string(key, provenance);
                    // A map cannot hold two identical keys, so a restored key
                    // landing on one already present would silently drop a
                    // field — a tool argument lost, where today's textual
                    // substitution serves both. Substituting instead yields
                    // duplicate property names, whose meaning is ambiguous. So
                    // the caller gets the document exactly as it came.
                    if out.contains_key(&restored) {
                        return None;
                    }
                    out.insert(restored, self.restore_document(item, provenance)?);
                }
                Value::Object(out)
            }
            other => other.clone(),
        })
    }
```

And beside them:

```rust
/// Characters that can end a JSON string or start an escape, so a value
/// carrying one cannot be substituted into JSON text byte-safely.
fn json_string_unsafe(character: char) -> bool {
    character == '"' || character == '\\' || character.is_control()
}
```

- [ ] **Step 4: Run them and watch them pass**

Run: `cd gateway && cargo test --quiet restore_in_string a_value_needing a_serialized_scalar a_token_this_request_did_not`
Expected: PASS.

- [ ] **Step 5: Prove it by mutation**

1. Make `json_string_unsafe` return `false` always — the structural path becomes unreachable. Expected: `a_value_needing_escaping_cannot_inject_fields_into_a_document` fails with three fields where one was asserted. **This is the mutation the integrity property rests on.**
2. Drop `provenance.restorable(candidate)` from the guard. Expected: `a_token_this_request_did_not_issue_is_left_where_it_stands` fails.
3. In `restore_document`'s object arm, replace the restored key with `key.clone()`. Expected: **the test below fails** — write it first; without it an implementation could reuse `restore_value`'s key check, regress the nested-key case to a refusal, and still pass every other test in this plan.

```rust
    #[test]
    fn a_placeholder_key_inside_a_nested_document_is_restored_as_it_is_today() {
        // The key rule keeps the depth it has. Refusing here would reject a
        // response served today: this string is text to `restore_value` now,
        // is substituted, and is served. Additivity decides it.
        let mut mapping = Mapping::default();
        mapping.begin_request();
        let token = mapping
            .mask("Martina Weber", &[span("PERSON", 0, 13)])
            .unwrap();
        let provenance = Provenance::new(mapping.issued(), HashSet::new());

        let restored =
            mapping.restore_in_string(&format!("{{\"{token}\":\"ok\"}}"), &provenance);
        assert_eq!(restored, "{\"Martina Weber\":\"ok\"}");
    }

    #[test]
    fn a_restored_key_that_would_collide_leaves_the_document_untouched() {
        // A map cannot hold two identical keys, so structural restoration
        // would silently drop one — a tool argument lost, where a textual
        // substitution today serves both. Substituting instead yields
        // duplicate property names, whose meaning is ambiguous. So the string
        // is left exactly as it came.
        let mut mapping = Mapping::default();
        mapping.begin_request();
        // A value needing escaping, so the structural path is the one taken.
        let value = "Martina \"Weber\"";
        let token = mapping
            .mask(value, &[span("PERSON", 0, value.chars().count())])
            .unwrap();
        let provenance = Provenance::new(mapping.issued(), HashSet::new());

        let document = format!("{{\"{token}\":1,\"Martina \\\"Weber\\\"\":2}}");
        assert_eq!(
            mapping.restore_in_string(&document, &provenance),
            document,
            "neither field may be lost"
        );
    }
```

4. Remove the collision check. Expected: `a_restored_key_that_would_collide_leaves_the_document_untouched` fails with one field where two were sent.

- [ ] **Step 6: Full verification and commit**

```bash
cd gateway && cargo test --quiet && cargo fmt --check && touch src/*.rs && cargo clippy --all-targets -- -D warnings
git add gateway/src/mapping.rs
git commit -m "feat(gateway): a restored value cannot inject fields into a document

The inserted value decides first and the surrounding string second. A value
with no quote, backslash or control character cannot close a string, so it
is substituted in place and formatting survives; one that can forces
parse-and-re-serialize, where it lands in a leaf and is escaped.

Asserting that the result still parses is what this replaces: restoring a
token in {\"name\":\"[X]\"} to a value carrying a quote and a comma yields
valid JSON with fields nobody sent."
```

---

### Task 5: `restore_sweep` over a whole body

**Files:**
- Modify: `gateway/src/mapping.rs`

**Interfaces:**
- Consumes: `restore_in_string`, `restore_document` (Task 4).
- Produces: `impl Mapping { pub fn restore_sweep(&self, value: &Value, provenance: &Provenance) -> Value }`. Task 6 calls it.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn the_sweep_restores_every_string_and_never_fails() {
        let mut mapping = Mapping::default();
        mapping.begin_request();
        let token = mapping
            .mask("Martina Weber", &[span("PERSON", 0, 13)])
            .unwrap();
        let provenance = Provenance::new(mapping.issued(), HashSet::new());

        let body = json!({
            "choices": [{"message": {
                "content": format!("hello {token}"),
                "refusal": format!("I cannot help with {token}"),
                "annotations": [{"url_citation": {"title": format!("{token} page")}}],
            }}],
            // A key, and a token nobody issued: both untouched, and neither
            // refuses. `restore_value` would have raised PlaceholderKey here.
            "trace": {"[PERSON_9]": "invented [ORG_4]"}
        });

        assert_eq!(
            mapping.restore_sweep(&body, &provenance),
            json!({
                "choices": [{"message": {
                    "content": "hello Martina Weber",
                    "refusal": "I cannot help with Martina Weber",
                    "annotations": [{"url_citation": {"title": "Martina Weber page"}}],
                }}],
                "trace": {"[PERSON_9]": "invented [ORG_4]"}
            })
        );
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd gateway && cargo test --quiet the_sweep_restores_every_string`
Expected: FAIL to compile — `restore_sweep` does not exist.

- [ ] **Step 3: Implement**

```rust
    /// The lenient pass over a whole response body.
    ///
    /// Infallible by type, which is the design's leniency written where it
    /// cannot be forgotten: **nothing in the sweep refuses**, including a
    /// placeholder-shaped key of either kind. Strictness lives in the slot path
    /// that runs after this and overwrites what it addresses.
    pub fn restore_sweep(&self, value: &Value, provenance: &Provenance) -> Value {
        self.restore_document(value, provenance)
    }
```

- [ ] **Step 4: Run it and watch it pass**

Run: `cd gateway && cargo test --quiet the_sweep_restores_every_string`
Expected: PASS.

- [ ] **Step 5: Prove it by mutation**

Change `restore_sweep` to call `self.restore_value(value).unwrap_or_else(|_| value.clone())`. Expected: the named test fails — `restore_value` raises `PlaceholderKey` on `[PERSON_9]`, so the whole body comes back unchanged and `content` is not restored. That is the difference between the two policies, made visible.

- [ ] **Step 6: Full verification and commit**

```bash
cd gateway && cargo test --quiet && cargo fmt --check && touch src/*.rs && cargo clippy --all-targets -- -D warnings
git add gateway/src/mapping.rs
git commit -m "feat(gateway): the sweep restores a whole body and never refuses

Infallible by type, which puts the leniency where it cannot be forgotten.
Strictness lives in the slot path that runs after it."
```

---

### Task 6: Wire it into `serve` — sweep first, then overwrite

**Files:**
- Modify: `gateway/src/proxy.rs`

**Interfaces:**
- Consumes: `Provenance`, `placeholder_literals`, `Mapping::begin_request`, `Mapping::issued`, `Mapping::restore_sweep`.

- [ ] **Step 1: Write the failing tests**

In `proxy.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn a_refusal_and_a_citation_title_reach_the_client_restored() {
        // Issue #31's two fields, asserted on what the CLIENT received.
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {
                "role": "assistant",
                "content": "ok",
                "refusal": "I cannot help with [PERSON_1]",
                "annotations": [{"url_citation": {"title": "[PERSON_1] page"}}]
            }}]}),
        )
        .await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": SECRET}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let text = serde_json::to_string(&body).unwrap();
        assert!(
            !text.contains("[PERSON_1]"),
            "a placeholder reached the client: {text}"
        );
    }

    #[tokio::test]
    async fn a_token_nobody_issued_in_an_undescribed_field_is_served_not_refused() {
        // Additivity, tested rather than asserted. "The suite stayed green"
        // only covers what the suite already tests.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {
                "role": "assistant",
                "content": "ok",
                "refusal": "invented [PERSON_9]"
            }}]}),
        )
        .await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["choices"][0]["message"]["refusal"], "invented [PERSON_9]");
    }
```

`SECRET`, `person_span`, `detector_returning`, `upstream_returning`, `state_with`, `test_limits` and `call` all exist. Read a neighbouring test and match the signatures rather than trusting this plan's memory of them.

- [ ] **Step 2: Run them and watch the first fail**

Run: `cd gateway && cargo test --quiet a_refusal_and_a_citation a_token_nobody_issued`
Expected: the first FAILS with `[PERSON_1]` in the client's body. The second may already pass — say so; it is a regression guard either way.

- [ ] **Step 3: Build the provenance in `handle`**

`handle` masks into `let mut work = guard.clone();`. Before masking, call `work.begin_request();`. After masking and before the upstream call, build:

```rust
    // Built from the request as it arrived, before anything rewrote it.
    let written = mapping::placeholder_literals(&body);
```

Thread `Provenance::new(work.issued(), written)` through to `serve` alongside the mapping it already receives. Read how `mapping` reaches `serve` today and follow that shape; do not introduce a second way of passing per-request state.

- [ ] **Step 4: Sweep, then overwrite, in `serve`**

Replace `let mut restored = upstream.clone();` with the sweep, and leave the slot loop exactly as it is:

```rust
    // The sweep runs on the untouched upstream body and the slots below are
    // computed from that same body, so neither pass reads the other's output
    // and no value is restored twice. Sweeping the *result* would corrupt a
    // multi-turn session: strict restoration inserts a caller's own literal
    // verbatim, and a second pass over it would replace that literal with an
    // earlier turn's value.
    //
    // Order matters the other way too: sweeping and stopping would make
    // `content` lenient, so an unmappable token there would be served instead
    // of refusing. The slot loop's strict result wins wherever the two overlap.
    let mut restored = mapping.restore_sweep(&upstream, &provenance);
```

- [ ] **Step 5: Run them and watch them pass**

Run: `cd gateway && cargo test --quiet a_refusal_and_a_citation a_token_nobody_issued`
Expected: PASS.

- [ ] **Step 6: Prove it by mutation**

1. Move the sweep after the slot loop, sweeping `restored` instead of `upstream`. Expected: Task 7's multi-turn test fails. If Task 7 is not written yet, write that test now — this ordering is the plan's most load-bearing decision and must not go in unpinned.
2. Delete the sweep entirely. Expected: `a_refusal_and_a_citation_title_reach_the_client_restored` fails.

- [ ] **Step 7: Full verification and commit**

```bash
cd gateway && cargo test --quiet && cargo fmt --check && touch src/*.rs && cargo clippy --all-targets -- -D warnings
git add gateway/src/proxy.rs
git commit -m "fix(gateway): a placeholder no longer reaches the client from an undescribed field

The buffered success path wrote only what it described, which is why an
undescribed field forwarded a token — refusal and a citation title among
them. It now sweeps the untouched upstream body first and overwrites each
described field with its strict restoration computed from that same body,
so neither pass reads the other's output."
```

---

### Task 7: The cases that hold the design in place

**Files:**
- Modify: `gateway/src/proxy.rs` (tests only), `README.md`

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: The multi-turn case, which is why the order is what it is**

```rust
    #[tokio::test]
    async fn a_caller_writing_a_session_owned_literal_gets_its_own_text_back() {
        // Turn one issues [PERSON_1]. Turn two's caller writes that literal
        // themselves; the provider echoes it. The client must receive what it
        // wrote, not turn one's value. This is the test that fails if the sweep
        // is ever moved after the slots.
        //
        // It also pins a divergence the spec records deliberately: the same
        // token is restored in `content` (described, strict, ambiguity
        // included — today's behaviour) and left in `refusal` (undescribed).
        // #32 is what makes them agree.
    }
```

Write it against the existing session helpers — find the test that drives two turns of one session and follow it. Assert both halves: the literal echoed into `refusal` comes back as the caller wrote it, and the divergence in `content` is asserted rather than discovered.

- [ ] **Step 2: Provider parity**

```rust
    #[tokio::test]
    async fn both_providers_treat_an_undescribed_response_field_the_same_way() {
        // The asymmetry is what made this bug: one provider was given a
        // treatment the other was not, and nothing compared them. This is the
        // only test here that catches the class rather than the instance.
    }
```

Drive one response shape carrying a token in an undescribed field through **both** providers and assert the same outcome.

- [ ] **Step 3: Run everything**

```bash
cd gateway && cargo test --quiet && cargo fmt --check && touch src/*.rs && cargo clippy --all-targets -- -D warnings
cd ../detector && uv run pytest -q && uv run ruff check . ../evaluation && uv run mypy src
cd .. && make check-entity-types && make check-layers && make check-base-install
```

- [ ] **Step 4: Narrow the README's promise**

`README.md` says no placeholder is ever handed to the client in place of a value. Correct it to what the code now does: a placeholder issued by this gateway does not reach the client from a field the gateway describes, and elsewhere everything this request issued and the caller did not write is restored. Name #32 as what restores the unqualified sentence.

Sweep the README for any other sentence about restoration that this changes, and report every claim you checked including the ones that held.

- [ ] **Step 5: Commit**

```bash
git add gateway/src/proxy.rs README.md
git commit -m "test(gateway): the order is pinned by a session, and the providers by a parity test

The multi-turn case is what killed the idempotence argument and is what
fails if the sweep is ever moved after the slots. The parity test is the
only one here that catches the class rather than the instance — one
provider given a treatment the other was not is what made this bug, and
three like it."
```

---

## Self-Review

**Spec coverage.** Sweep-then-overwrite → Task 6. Provenance and the two sets → Tasks 1–3. The value-first JSON rule and its recursion → Task 4. Leniency, including both key rules → Tasks 4 and 5. Every described field keeping its slot → Task 6 leaves the slot loop untouched, deliberately. Anthropic's refusals unchanged → no task touches `provider.rs`. The README's narrowed promise → Task 7. The spec's nine testing items map to Tasks 4, 5, 6 and 7.

**Deliberately not in this plan**, per the spec: unifying the three restoration policies; Anthropic's response-side refusals; #32.

**Type consistency.** `Provenance::new(HashSet<String>, HashSet<String>)` and `restorable(&str) -> bool` are defined in Task 1 and used in 4, 5 and 6. `placeholder_literals(&Value) -> HashSet<String>` is defined in Task 2 and called in Task 6. `begin_request` / `issued` are defined in Task 3 and called in Task 6. `restore_in_string(&str, &Provenance) -> String` is defined in Task 4 and used by `restore_document` and `restore_sweep`. `restore_sweep(&Value, &Provenance) -> Value` is defined in Task 5 and called in Task 6.

**Known rough edges.** Task 6 threads a new value into `serve`; read how the mapping reaches it today rather than inventing a second mechanism. Task 4's third mutation is expected to fail to fail — that outcome is the finding, and the task says to write the missing test rather than move on. Task 7's two tests are described rather than written out because both must be built on session and provider helpers whose exact shapes should be read from neighbouring tests; every other test in this plan is given in full.
