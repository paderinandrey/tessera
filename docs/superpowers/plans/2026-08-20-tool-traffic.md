# Tool Traffic Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Mask tool definitions, tool call arguments and tool results on the non-streaming path, so a coding agent whose results are small can use this gateway end to end.

**Architecture:** A location a provider describes gains a kind. `Slot::Text` is today's behaviour; `Slot::Json` says the value there is a document whose string leaves get masked and whose keys never do. Masking is split in two because detection is asynchronous and `Mapping` is not: a bounded walk collects the leaves, `mask_all` detects them, and a second walk puts the masked strings back in the same order.

**Tech Stack:** Rust, `serde_json`, `axum`, `wiremock` for provider tests.

## Global Constraints

- **Every invariant is proved by mutation.** Break it, watch the specific test fail, record the observed output in the task report, restore. A mutation that fails no test is a finding to report, not a mutation to adjust until it bites.
- **Keys are never masked.** The walk touches values only. A tool name, a schema property name, a `tool_call_id` and `tool_choice`'s selector are the client's dispatch.
- **Nothing degrades.** Every failure in this slice is a refusal issued *before* the upstream call.
- **Verification before each commit,** from `gateway/`: `cargo test`, `cargo fmt --check`, and `cargo clippy --all-targets -- -D warnings` after `touch src/*.rs`. An incremental clippy run hides a stale suppression; this branch's predecessor lost two review rounds to that.
- **Commit signing** uses a 1Password SSH agent that locks intermittently. Never work around it — no `--no-gpg-sign`, no git config changes. Preserve a patch and report the block.
- Stage files by name. Never `git add -A`.

**Deviation from the spec, recorded here rather than discovered later.** The design sketches one `Mapping::mask_value`. This plan implements it as two free functions in `mapping.rs`, `json_leaves` and `replace_text_leaves`, because detection sits between them and a single method would have to be either async or given a pre-built map keyed by something. Ordered leaves need no key at all. The design's intent — one walk, mirroring `restore_value`, keys untouched — is unchanged.

---

### Task 1: A location gains a kind

Pure refactor. Every existing test must pass unchanged at the end of it; nothing about what the gateway masks changes here.

**Files:**
- Modify: `gateway/src/provider.rs`, `gateway/src/proxy.rs`

**Interfaces:**
- Produces: `pub enum Slot`, and `Provider::request_pointers`/`response_pointers` returning `Result<Vec<Slot>, ShapeError>`. Every later task consumes these.

- [ ] **Step 1: Write the failing test**

In `provider.rs`'s `mod tests`, the existing helper `fn pointers(...)` collapses slots to strings. Add beside it:

```rust
    #[test]
    fn a_text_slot_names_the_pointer_it_wraps() {
        let slot = Slot::Text("/messages/0/content".to_owned());
        assert_eq!(slot.pointer(), "/messages/0/content");
    }

    #[test]
    fn a_json_slot_remembers_whether_the_document_is_embedded() {
        let plain = Slot::Json {
            pointer: "/messages/0/content/0/input".to_owned(),
            embedded: false,
        };
        let embedded = Slot::Json {
            pointer: "/messages/0/tool_calls/0/function/arguments".to_owned(),
            embedded: true,
        };
        assert_eq!(plain.pointer(), "/messages/0/content/0/input");
        assert!(!matches!(plain, Slot::Json { embedded: true, .. }));
        assert!(matches!(embedded, Slot::Json { embedded: true, .. }));
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd gateway && cargo test --quiet slot`
Expected: FAIL — `cannot find type Slot in this scope`.

- [ ] **Step 3: Add the type**

In `provider.rs`, beside `TextSlot`:

```rust
/// Where a maskable value lives, and what kind of value it is. `Text` is a
/// string masked as it stands. `Json` is a document whose string leaves are
/// masked and whose keys are not — `embedded` distinguishes a document from a
/// string holding one, which is the only difference between Anthropic's
/// `input` object and OpenAI's `arguments`.
#[derive(Debug, Clone, PartialEq)]
pub enum Slot {
    Text(String),
    Json { pointer: String, embedded: bool },
}

impl Slot {
    pub fn pointer(&self) -> &str {
        match self {
            Slot::Text(pointer) => pointer,
            Slot::Json { pointer, .. } => pointer,
        }
    }
}
```

- [ ] **Step 4: Change the trait**

In `provider.rs`, change both signatures:

```rust
    fn request_pointers(&self, body: &Value) -> Result<Vec<Slot>, ShapeError>;
    fn response_pointers(&self, body: &Value) -> Result<Vec<Slot>, ShapeError>;
```

Then change every `out: &mut Vec<String>` helper — `content_pointers`, `identifier_pointer` — to `out: &mut Vec<Slot>`, and every `out.push(x)` to `out.push(Slot::Text(x))`. Both providers' `request_pointers` and `response_pointers` bodies change the same way: `Vec::new()` still, `Slot::Text(...)` pushed.

- [ ] **Step 5: Teach the two call sites to dispatch**

`proxy.rs`, in `mask_all`, replace the loop head:

```rust
    for slot in slots {
        let pointer = match slot {
            Slot::Text(pointer) => pointer,
            // No provider emits this until Task 4. Refused rather than
            // `unreachable!`: a panic in a request handler takes the process
            // and every live session's mapping with it, and "this cannot
            // happen" is the claim a refusal costs nothing to stop trusting.
            Slot::Json { .. } => return Err(ShapeError::Request(provider.name()).into()),
        };
        let text = read_pointer(body, pointer)?;
```

`proxy.rs`, the buffered response loop near line 391:

```rust
    for slot in provider.response_pointers(&upstream)? {
        match slot {
            Slot::Text(pointer) => {
                let text = read_pointer(&upstream, &pointer)?;
                write_pointer(&mut restored, &pointer, &mapping.restore(&text)?)?;
            }
            Slot::Json { .. } => return Err(ShapeError::Response(provider.name()).into()),
        }
    }
```

Rename `mask_all`'s `pointers` parameter to `slots` and give it type `&[Slot]`.

- [ ] **Step 6: Fix the test helper**

`provider.rs`'s `mod tests` helper becomes:

```rust
    fn pointers(slots: Result<Vec<Slot>, ShapeError>) -> Vec<String> {
        slots
            .unwrap()
            .into_iter()
            .map(|slot| slot.pointer().to_owned())
            .collect()
    }
```

- [ ] **Step 7: Run everything**

Run: `cd gateway && cargo test --quiet && cargo fmt --check && touch src/*.rs && cargo clippy --all-targets -- -D warnings`
Expected: every existing test passes. If any test's expectations changed, the refactor was not pure — stop and report rather than editing the test.

- [ ] **Step 8: Prove it by mutation**

Change `Slot::pointer` for the `Json` arm to return `""`.
Run: `cd gateway && cargo test --quiet a_json_slot_remembers`
Expected: FAIL on the `pointer()` assertion. Restore.

- [ ] **Step 9: Commit**

```bash
git add gateway/src/provider.rs gateway/src/proxy.rs
git commit -m "refactor(gateway): a described location gains a kind

Providers name where maskable values live, and masking is written once
against those names. Tool arguments are not a string, so a name alone can
no longer say what to do with what it points at. Behaviour is unchanged:
every location is still Text, and the Json arm refuses until the task
that emits one."
```

---

### Task 2: A bounded walk over a document

Pure functions, unit-tested, wired to nothing.

**Files:**
- Modify: `gateway/src/mapping.rs`

**Interfaces:**
- Produces: `pub enum Leaf { Text(String), Number(String) }`, `pub fn json_leaves(value: &Value) -> Result<Vec<Leaf>, MappingError>`, `pub fn replace_text_leaves(value: &Value, masked: &[String]) -> Result<Value, MappingError>`, and `MappingError::TooDeep` / `MappingError::TooLarge`.

- [ ] **Step 1: Write the failing tests**

In `mapping.rs`'s `mod tests`:

```rust
    #[test]
    fn a_walk_reports_string_leaves_in_order_and_ignores_keys() {
        let document = json!({
            "b_second": "Weber",
            "a_first": {"nested": "Meier"},
            "list": ["Schmidt", 7, true, null]
        });
        let leaves = json_leaves(&document).unwrap();
        assert_eq!(
            leaves,
            vec![
                Leaf::Text("Meier".to_owned()),
                Leaf::Text("Weber".to_owned()),
                Leaf::Text("Schmidt".to_owned()),
                Leaf::Number("7".to_owned()),
            ],
            "keys are not leaves, and booleans and null carry no personal data"
        );
    }

    #[test]
    fn replacement_puts_masked_text_back_in_walk_order() {
        let document = json!({"who": "Weber", "n": 7, "where": "Bern"});
        let masked = vec!["[PERSON_1]".to_owned(), "[LOCATION_2]".to_owned()];
        let result = replace_text_leaves(&document, &masked).unwrap();
        assert_eq!(
            result,
            json!({"who": "[PERSON_1]", "n": 7, "where": "[LOCATION_2]"}),
            "numbers keep their type and their value"
        );
    }

    #[test]
    fn a_document_past_the_depth_bound_is_refused() {
        let mut document = json!("Weber");
        for _ in 0..(MAX_JSON_DEPTH + 1) {
            document = json!([document]);
        }
        assert!(matches!(
            json_leaves(&document),
            Err(MappingError::TooDeep)
        ));
    }

    #[test]
    fn a_document_past_the_node_bound_is_refused() {
        let wide: Vec<Value> = (0..=MAX_JSON_NODES).map(|n| json!(n)).collect();
        assert!(matches!(
            json_leaves(&Value::Array(wide)),
            Err(MappingError::TooLarge)
        ));
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cd gateway && cargo test --quiet json_leaves replace_text_leaves a_document_past`
Expected: FAIL — `cannot find function json_leaves in this scope`.

- [ ] **Step 3: Add the error variants**

In `mapping.rs`'s `MappingError`:

```rust
    #[error("a tool document is nested deeper than this gateway will walk")]
    TooDeep,
    #[error("a tool document carries more values than this gateway will walk")]
    TooLarge,
```

- [ ] **Step 4: Write the walk**

In `mapping.rs`, beside `restore_value`:

```rust
/// How deep a client's document may nest, and how many values it may hold.
/// `restore_value` needs neither: it walks a provider's own response envelope,
/// whose shape is the provider's business. This walk is reachable from a
/// client's tool arguments, which are untrusted input — a document nested ten
/// thousand deep would end the process by exhausting the stack, and the
/// recursion below is the one that would do it.
pub const MAX_JSON_DEPTH: usize = 64;
pub const MAX_JSON_NODES: usize = 10_000;

/// A value a document carries, in the order the walk finds it. Keys are absent
/// by construction: this walk descends into values and never yields a name.
#[derive(Debug, Clone, PartialEq)]
pub enum Leaf {
    Text(String),
    /// Rendered as the client wrote it. A card number, a German tax ID and a
    /// French NIR are digits alone, so a number has to be looked at — but it is
    /// never replaced, because a schema that declared a number may reject a
    /// string.
    Number(String),
}

pub fn json_leaves(value: &Value) -> Result<Vec<Leaf>, MappingError> {
    let mut leaves = Vec::new();
    let mut nodes = 0usize;
    walk(value, 0, &mut nodes, &mut leaves)?;
    Ok(leaves)
}

fn walk(
    value: &Value,
    depth: usize,
    nodes: &mut usize,
    leaves: &mut Vec<Leaf>,
) -> Result<(), MappingError> {
    if depth > MAX_JSON_DEPTH {
        return Err(MappingError::TooDeep);
    }
    *nodes += 1;
    if *nodes > MAX_JSON_NODES {
        return Err(MappingError::TooLarge);
    }
    match value {
        Value::String(text) => leaves.push(Leaf::Text(text.clone())),
        Value::Number(number) => leaves.push(Leaf::Number(number.to_string())),
        Value::Array(items) => {
            for item in items {
                walk(item, depth + 1, nodes, leaves)?;
            }
        }
        Value::Object(fields) => {
            // `fields` is iterated, never yielded: a key is the client's
            // dispatch, and masking one would break the call it dispatches.
            for (_key, item) in fields {
                walk(item, depth + 1, nodes, leaves)?;
            }
        }
        Value::Bool(_) | Value::Null => {}
    }
    Ok(())
}

/// The mirror of `restore_value`, and the second half of masking: the walk
/// above collected the leaves, `mask_all` detected them, and this puts the
/// masked strings back where they came from. Order is the only correspondence,
/// which is why both walks are the same function shape.
pub fn replace_text_leaves(value: &Value, masked: &[String]) -> Result<Value, MappingError> {
    let mut next = 0usize;
    let mut nodes = 0usize;
    let result = replace(value, 0, &mut nodes, masked, &mut next)?;
    if next != masked.len() {
        return Err(MappingError::BadSpan("more masked strings than text leaves"));
    }
    Ok(result)
}

fn replace(
    value: &Value,
    depth: usize,
    nodes: &mut usize,
    masked: &[String],
    next: &mut usize,
) -> Result<Value, MappingError> {
    if depth > MAX_JSON_DEPTH {
        return Err(MappingError::TooDeep);
    }
    *nodes += 1;
    if *nodes > MAX_JSON_NODES {
        return Err(MappingError::TooLarge);
    }
    Ok(match value {
        Value::String(_) => {
            let replacement = masked
                .get(*next)
                .ok_or(MappingError::BadSpan("fewer masked strings than text leaves"))?;
            *next += 1;
            Value::String(replacement.clone())
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| replace(item, depth + 1, nodes, masked, next))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| Ok((key.clone(), replace(item, depth + 1, nodes, masked, next)?)))
                .collect::<Result<serde_json::Map<_, _>, MappingError>>()?,
        ),
        other => other.clone(),
    })
}
```

- [ ] **Step 5: Run the tests**

Run: `cd gateway && cargo test --quiet`
Expected: PASS, including every pre-existing test.

- [ ] **Step 6: Prove each invariant by mutation**

Four mutations, each restored before the next, each with its observed output recorded in your report:

1. Make `walk`'s `Object` arm push `Leaf::Text(key.clone())` before descending. Expected: `a_walk_reports_string_leaves_in_order_and_ignores_keys` fails — this is the keys-are-never-masked invariant, and it is the most important one in the slice.
2. Change `if depth > MAX_JSON_DEPTH` to `if depth > MAX_JSON_DEPTH * 2`. Expected: `a_document_past_the_depth_bound_is_refused` fails.
3. Change the node check to `*nodes > MAX_JSON_NODES * 2`. Expected: `a_document_past_the_node_bound_is_refused` fails.
4. Make `replace`'s `Number` case substitute from `masked` too. Expected: `replacement_puts_masked_text_back_in_walk_order` fails, because the number's type changes.

- [ ] **Step 7: Commit**

```bash
git add gateway/src/mapping.rs
git commit -m "feat(gateway): a bounded walk over a client's document

The mirror of restore_value, split in two because detection sits between
the halves and Mapping is not async. Ordered leaves need no key, so the
correspondence between the walks is their shape.

Bounded because restore_value never had to be: it walks a provider's own
envelope, and this walks a client's tool arguments."
```

---

### Task 3: The size bound's configuration

**Files:**
- Modify: `gateway/src/config.rs`, `gateway/tessera.example.toml`, `deploy/tessera.container.toml`, `deploy/tessera.demo.toml`

**Interfaces:**
- Consumes: nothing.
- Produces: `Config::max_tool_bytes: usize`. Task 4 wires it.

- [ ] **Step 1: Write the failing tests**

In `config.rs`'s `mod tests`:

```rust
    #[test]
    fn the_tool_bound_has_a_default() {
        let config: Config = toml::from_str(MINIMAL).unwrap();
        assert_eq!(config.max_tool_bytes, 10_000);
    }

    #[test]
    fn a_zero_tool_bound_is_rejected() {
        let text = format!("{MINIMAL}\nmax_tool_bytes = 0\n");
        assert!(matches!(
            Config::from_toml(&text),
            Err(ConfigError::ZeroToolBytes)
        ));
    }
```

`MINIMAL` is the fixture the surrounding tests already use — read one of them and use whatever they use rather than inventing a second fixture.

- [ ] **Step 2: Run them and watch them fail**

Run: `cd gateway && cargo test --quiet tool_bound`
Expected: FAIL — no field `max_tool_bytes`.

- [ ] **Step 3: Add the key**

In `config.rs`:

```rust
    #[error("max_tool_bytes must be greater than zero")]
    ZeroToolBytes,
```

```rust
    /// How many bytes of tool structure one request may newly scan, summed
    /// across every tool definition, argument and result in it.
    ///
    /// This key and `detector_timeout_secs` are one constraint written twice.
    /// The detector clears roughly 530 characters per wall-clock second, so
    /// 10 000 characters is about 19 seconds against a 30-second timeout, with
    /// room for a slower machine. Raise the timeout to serve larger results and
    /// this has to rise with it; lower the timeout and this has to fall.
    ///
    /// It is not derived from the timeout automatically on purpose: a derived
    /// default changes behaviour silently when an unrelated key moves.
    ///
    /// The ceiling exists because the detection cache does not help the first
    /// time a text is seen, and a tool result is usually seen once. Issue #28
    /// carries the work that lifts it.
    #[serde(default = "default_max_tool_bytes")]
    pub max_tool_bytes: usize,
```

```rust
fn default_max_tool_bytes() -> usize {
    10_000
}
```

And in `Config::from_toml`'s validation, beside the existing checks:

```rust
        if config.max_tool_bytes == 0 {
            return Err(ConfigError::ZeroToolBytes);
        }
```

- [ ] **Step 4: Run the tests**

Run: `cd gateway && cargo test --quiet`
Expected: PASS.

- [ ] **Step 5: Document it in the three TOMLs**

Add to `gateway/tessera.example.toml`, `deploy/tessera.container.toml` and `deploy/tessera.demo.toml`, matching each file's existing voice — read the `detection_cache_entries` entry in each and write in the same register. The comment must carry the arithmetic, the relationship to `detector_timeout_secs`, and the fact that a large tool result is refused rather than served slowly.

- [ ] **Step 6: Prove it by mutation**

Change `default_max_tool_bytes` to return `20_000`.
Expected: `the_tool_bound_has_a_default` fails. Restore.

Delete the zero check from `from_toml`.
Expected: `a_zero_tool_bound_is_rejected` fails. Restore.

- [ ] **Step 7: Commit**

```bash
git add gateway/src/config.rs gateway/tessera.example.toml deploy/tessera.container.toml deploy/tessera.demo.toml
git commit -m "feat(gateway): a bound on the tool structure one request scans

The detection cache does not help the first time a text is seen, and a
tool result is usually seen once. At the default timeout the ceiling is
about 10 000 characters, which passes ordinary agent traffic and refuses
a large file read honestly. Issue #28 lifts it."
```

---

### Task 4: Anthropic tool traffic, end to end

The first task that emits a `Json` slot, relaxes a refusal, and can be tested through the proxy.

**Files:**
- Modify: `gateway/src/provider.rs`, `gateway/src/proxy.rs`

**Interfaces:**
- Consumes: `Slot`, `json_leaves`, `replace_text_leaves`, `Config::max_tool_bytes`.
- Produces: `mask_all` handling both slot kinds; the `stream: true` + tool refusal.

- [ ] **Step 1: Write the failing tests**

In `provider.rs`'s `mod tests`:

```rust
    #[test]
    fn anthropic_describes_tool_definitions_and_calls_and_results() {
        let body = json!({
            "model": "claude",
            "tools": [{
                "name": "read_file",
                "description": "Read a file for Dr. Weber",
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}
            }],
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "read_file",
                     "input": {"path": "/home/weber/notes.txt"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "Martina Weber"}
                ]}
            ]
        });
        assert_eq!(
            pointers(Anthropic.request_pointers(&body)),
            vec![
                "/tools/0/description",
                "/tools/0/input_schema",
                "/messages/0/content/0/input",
                "/messages/1/content/0/content",
            ]
        );
    }

    #[test]
    fn anthropic_never_describes_a_tool_name_or_a_result_id() {
        let body = json!({
            "model": "claude",
            "tools": [{"name": "read_file", "description": "d", "input_schema": {}}],
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "x"}
            ]}]
        });
        let described = pointers(Anthropic.request_pointers(&body));
        assert!(
            !described.iter().any(|p| p.ends_with("/name") || p.ends_with("/tool_use_id")),
            "a tool name and a result id are the client's dispatch: {described:?}"
        );
    }

    #[test]
    fn anthropic_refuses_tool_traffic_on_a_streamed_request() {
        let body = json!({
            "model": "claude",
            "stream": true,
            "tools": [{"name": "read_file", "description": "d", "input_schema": {}}],
            "messages": [{"role": "user", "content": "hello"}]
        });
        assert!(matches!(
            Anthropic.request_pointers(&body),
            Err(ShapeError::Unsupported("anthropic", "streamed tool traffic"))
        ));
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cd gateway && cargo test --quiet anthropic_describes anthropic_never_describes anthropic_refuses_tool`
Expected: FAIL — the first two because tool fields are still refused outright, the third because the refusal does not exist yet.

- [ ] **Step 3: Add the streamed-tool refusal**

In `provider.rs`, a free function beside `reject_tool_fields`:

```rust
/// `proxy.rs` calls `request_pointers` before it looks at `stream`, so relaxing
/// the tool refusal admits streamed tool requests as readily as buffered ones —
/// and `stream_slots`, which this slice does not touch, would then reject the
/// tool events *after* the upstream call, spending the caller's tokens to
/// return a broken stream. The streaming slice deletes this function; nothing
/// else should.
fn reject_streamed_tools(
    body: &Value,
    carries_tools: bool,
    provider: &'static str,
) -> Result<(), ShapeError> {
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if streaming && carries_tools {
        return Err(ShapeError::Unsupported(provider, "streamed tool traffic"));
    }
    Ok(())
}
```

- [ ] **Step 4: Describe Anthropic's tool locations**

In `provider.rs`, a helper both providers will use — OpenAI takes it in Task 5:

```rust
/// A tool definition's prose and its schema. The schema goes in whole rather
/// than by naming the keywords that carry text: `description` is not the only
/// one — `default`, `const`, `enum`, `examples`, `title` and `$comment` are all
/// client-controlled strings — and a list of what to scan is wrong the day
/// someone adds to it. Property names are keys, which the walk never yields.
fn tool_definition_slots(
    prefix: &str,
    definition: &Value,
    schema_field: &str,
    out: &mut Vec<Slot>,
) {
    if definition.get("description").and_then(Value::as_str).is_some() {
        out.push(Slot::Text(format!("{prefix}/description")));
    }
    if definition.get(schema_field).is_some() {
        out.push(Slot::Json {
            pointer: format!("{prefix}/{schema_field}"),
            embedded: false,
        });
    }
}
```

In `Anthropic::request_pointers`, replace the unconditional `reject_tool_fields(body, "anthropic")?` with:

```rust
        let carries_tools = body.get("tools").is_some_and(|value| !value.is_null());
        reject_streamed_tools(body, carries_tools, "anthropic")?;
        if let Some(tools) = body.get("tools") {
            let tools = tools.as_array().ok_or(ShapeError::Request("anthropic"))?;
            for (index, definition) in tools.iter().enumerate() {
                tool_definition_slots(
                    &format!("/tools/{index}"),
                    definition,
                    "input_schema",
                    &mut pointers,
                );
            }
        }
```

Note this needs `let mut pointers = Vec::new();` to move above it — it currently sits below the `thinking` check. Move the declaration, not the checks.

Then, in `content_pointers`, teach the array arm the two tool block types before it falls through to the refusal:

```rust
                match part.get("type").and_then(Value::as_str).unwrap_or("") {
                    "tool_use" => {
                        if part.get("input").is_some() {
                            out.push(Slot::Json {
                                pointer: format!("{prefix}/{index}/input"),
                                embedded: false,
                            });
                        }
                        continue;
                    }
                    "tool_result" => {
                        if let Some(content) = part.get("content") {
                            content_pointers(
                                &format!("{prefix}/{index}/content"),
                                content,
                                provider,
                                out,
                            )?;
                        }
                        continue;
                    }
                    kind if UNSCANNED_PART_TYPES.contains(&kind) => continue,
                    _ => return Err(ShapeError::Request(provider)),
                }
```

`tool_result.content` recurses through `content_pointers` itself, which is what makes a string result and a list of blocks one case rather than two — and what makes an image inside a result inherit the policy images already have.

Finally, remove `"tools"`, `"tool_choice"` and `"tool_calls"` from `TOOL_FIELDS`, and delete the `reject_tool_fields(message, "anthropic")` call for messages. Update `TOOL_FIELDS`'s doc comment: it now lists what is still refused and why, which is OpenAI's `functions` and `function_call` until Task 5.

- [ ] **Step 5: Teach `mask_all` the Json kind**

In `proxy.rs`, replace the Task 1 refusal arm in `mask_all`:

```rust
        match slot {
            Slot::Text(pointer) => {
                let text = read_pointer(body, pointer)?;
                let spans = detector.detect(&text, credential).await?;
                total += spans.len();
                count_distinct(&text, &spans, &mut distinct);
                write_pointer(&mut masked, pointer, &mapping.mask(&text, &spans)?)?;
            }
            Slot::Json { pointer, embedded } => {
                let document = read_document(body, pointer, *embedded)?;
                let leaves = mapping::json_leaves(&document)?;
                let mut replacements = Vec::new();
                for leaf in &leaves {
                    match leaf {
                        mapping::Leaf::Text(text) => {
                            let spans = detector.detect(text, credential).await?;
                            total += spans.len();
                            count_distinct(text, &spans, &mut distinct);
                            replacements.push(mapping.mask(text, &spans)?);
                        }
                        // Task 6 refuses this when it carries a span. Until
                        // then a number is looked at by nobody, which is the
                        // behaviour that task exists to change.
                        mapping::Leaf::Number(_) => {}
                    }
                }
                let rebuilt = mapping::replace_text_leaves(&document, &replacements)?;
                write_document(&mut masked, pointer, &rebuilt, *embedded)?;
            }
        }
```

`count_distinct` is the existing block inside `mask_all` that fills `distinct`; extract it to a free function first, unchanged, so both arms use one copy rather than two that can drift.

Add to `provider.rs`:

```rust
/// A `Json` slot's document. `embedded` means the pointer addresses a string
/// holding a document rather than the document itself — OpenAI's `arguments`
/// against Anthropic's `input`.
pub fn read_document(body: &Value, pointer: &str, embedded: bool) -> Result<Value, ShapeError> {
    let at = body
        .pointer(pointer)
        .ok_or_else(|| ShapeError::Pointer(pointer.to_owned()))?;
    if !embedded {
        return Ok(at.clone());
    }
    let text = at
        .as_str()
        .ok_or_else(|| ShapeError::Pointer(pointer.to_owned()))?;
    serde_json::from_str(text).map_err(|_| ShapeError::Pointer(pointer.to_owned()))
}

pub fn write_document(
    body: &mut Value,
    pointer: &str,
    document: &Value,
    embedded: bool,
) -> Result<(), ShapeError> {
    let slot = body
        .pointer_mut(pointer)
        .ok_or_else(|| ShapeError::Pointer(pointer.to_owned()))?;
    *slot = if embedded {
        Value::String(serde_json::to_string(document).map_err(|_| ShapeError::Response("json"))?)
    } else {
        document.clone()
    };
    Ok(())
}
```

- [ ] **Step 6: Teach the response loop the Json kind**

In `proxy.rs`'s buffered response loop, replace its Task 1 refusal arm:

```rust
            Slot::Json { pointer, embedded } => {
                let document = read_document(&upstream, &pointer, embedded)?;
                let restored_document = mapping.restore_value(&document)?;
                write_document(&mut restored, &pointer, &restored_document, embedded)?;
            }
```

And in `Anthropic::response_pointers`, describe `tool_use` blocks in the response the same way the request does — read the existing function and follow its shape rather than inventing a second one.

- [ ] **Step 7: Wire the size bound**

In `proxy.rs`, before the masking loop:

```rust
    // Every tool structure this request newly scans, summed. Arguments count,
    // not only results: `Write` and `Edit` carry whole files in arguments, and
    // a tool call the model produced is restored to real values and echoed back
    // in the next turn's history — text the cache has never seen, because the
    // cache holds the masked request rather than the restored response.
    let tool_bytes: usize = slots
        .iter()
        .filter_map(|slot| match slot {
            Slot::Json { pointer, .. } => body.pointer(pointer),
            Slot::Text(_) => None,
        })
        .map(|value| value.to_string().len())
        .sum();
    if tool_bytes > limits.max_tool_bytes {
        return Err(ProxyError::ToolTooLarge);
    }
```

Add `ToolTooLarge` to `ProxyError` with the same 4xx/5xx treatment its neighbours get — read how `ShapeError::Unsupported` is turned into a response and match it.

- [ ] **Step 8: Write the proxy round-trip test**

In `proxy.rs`'s `mod tests`, using the helpers already there:

```rust
    #[tokio::test]
    async fn a_tool_call_is_masked_going_up_and_restored_coming_back() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [
                {"type": "tool_use", "id": "t1", "name": "read_file",
                 "input": {"path": "[PERSON_1]"}}
            ]}),
        )
        .await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let body = json!({
            "model": "claude",
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "read_file",
                 "input": {"path": "Weber"}}
            ]}]
        });
        let (status, returned) = call(state, "/v1/messages", body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            returned["content"][0]["input"]["path"], "Weber",
            "the client executes this, so it has to be the real value"
        );
        assert_eq!(
            returned["content"][0]["name"], "read_file",
            "the tool name is dispatch and is never touched"
        );
    }
```

`person_span()`, `detector_returning`, `upstream_returning`, `state_with`, `test_limits` and `call` all exist — read a neighbouring test and match its call shape rather than assuming these signatures.

- [ ] **Step 9: Run everything**

Run: `cd gateway && cargo test --quiet && cargo fmt --check && touch src/*.rs && cargo clippy --all-targets -- -D warnings`

- [ ] **Step 10: Prove each invariant by mutation**

Each restored before the next, each with observed output recorded:

1. Make `walk`'s `Object` arm yield keys (as in Task 2). Expected: the round-trip test now fails too, because `path` would be masked — the invariant is guarded at both levels.
2. Delete the `reject_streamed_tools` call. Expected: `anthropic_refuses_tool_traffic_on_a_streamed_request` fails.
3. In `tool_definition_slots`, emit the schema as `Slot::Text` instead of `Slot::Json`. Expected: a test fails — if none does, that is a finding: it means nothing pins that a schema's nested strings are reached, and the task needs a test that they are.
4. Set `tool_bytes` to `0` unconditionally. Expected: whichever test guards the bound fails — if none exists yet, write one here rather than reporting the mutation as passing.

- [ ] **Step 11: Commit**

```bash
git add gateway/src/provider.rs gateway/src/proxy.rs
git commit -m "feat(gateway): mask Anthropic tool traffic on the buffered path

Definitions, tool_use.input and tool_result content, described as
locations like everything else. The schema goes in whole rather than by
naming the keywords that carry prose, because a list of what matters is
wrong the day someone adds to it.

Streamed tool traffic is refused before the upstream call: request
pointers are collected before proxy.rs looks at stream, so relaxing the
refusal would otherwise admit a request that stream_slots rejects after
the caller has paid for it."
```

---

### Task 5: OpenAI tool traffic, end to end

**Files:**
- Modify: `gateway/src/provider.rs`

**Interfaces:**
- Consumes: everything from Task 4, including `tool_definition_slots` and `reject_streamed_tools`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn openai_describes_definitions_and_arguments_and_tool_messages() {
        let body = json!({
            "model": "gpt",
            "tools": [{"type": "function", "function": {
                "name": "read_file",
                "description": "Read a file for Dr. Weber",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
            }}],
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "t1", "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"/home/weber\"}"}
                }]},
                {"role": "tool", "tool_call_id": "t1", "content": "Martina Weber"}
            ]
        });
        assert_eq!(
            pointers(OpenAi.request_pointers(&body)),
            vec![
                "/tools/0/function/description",
                "/tools/0/function/parameters",
                "/messages/0/tool_calls/0/function/arguments",
                "/messages/1/content",
            ]
        );
    }

    #[test]
    fn openai_marks_arguments_as_an_embedded_document() {
        let body = json!({
            "model": "gpt",
            "messages": [{"role": "assistant", "content": null, "tool_calls": [{
                "id": "t1", "type": "function",
                "function": {"name": "f", "arguments": "{\"a\":\"Weber\"}"}
            }]}]
        });
        let slots = OpenAi.request_pointers(&body).unwrap();
        assert!(
            slots.iter().any(|slot| matches!(
                slot,
                Slot::Json { embedded: true, pointer } if pointer.ends_with("/arguments")
            )),
            "arguments is a string holding a document, not a document"
        );
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cd gateway && cargo test --quiet openai_describes openai_marks_arguments`
Expected: FAIL — tool fields still refused for OpenAI.

- [ ] **Step 3: Describe OpenAI's tool locations**

In `OpenAi::request_pointers`: move `let mut pointers = Vec::new();` above the checks as Task 4 did for Anthropic, call `reject_streamed_tools`, walk `tools` with `tool_definition_slots(&format!("/tools/{index}/function"), function, "parameters", &mut pointers)`, and per message: describe `tool_calls[].function.arguments` as `Slot::Json { embedded: true }`, and treat a `role: "tool"` message's `content` through `content_pointers` exactly as any other message's.

Delete the `tool_call_id` refusal — the id is dispatch and is now simply not described. Empty `TOOL_FIELDS` if nothing remains in it, and delete `reject_tool_fields` with it rather than leaving a function no caller uses.

- [ ] **Step 4: Describe them in the response too**

In `OpenAi::response_pointers`, describe `choices[].message.tool_calls[].function.arguments` as `Slot::Json { embedded: true }`. The existing refusal of `tool_calls` and `function_call` on the response side goes with it — read that code and remove only what this task replaces.

- [ ] **Step 5: Run everything**

Run: `cd gateway && cargo test --quiet && cargo fmt --check && touch src/*.rs && cargo clippy --all-targets -- -D warnings`

- [ ] **Step 6: Prove it by mutation**

1. Set `embedded: false` for `arguments`. Expected: `openai_marks_arguments_as_an_embedded_document` fails, and so does any round-trip test — a document read as a string is not a document.
2. Describe `/messages/1/tool_call_id`. Expected: a test fails — if none does, write one, because a masked `tool_call_id` breaks the client's dispatch silently.

- [ ] **Step 7: Commit**

```bash
git add gateway/src/provider.rs
git commit -m "feat(gateway): mask OpenAI tool traffic on the buffered path

arguments is a string holding a document rather than a document, which is
the whole of the difference from Anthropic and is carried by one flag.
tool_call_id stops being refused and starts being ignored: it is dispatch,
so describing it would break the call it dispatches."
```

---

### Task 6: A number that carries personal data refuses the request

**Files:**
- Modify: `gateway/src/proxy.rs`

**Interfaces:**
- Consumes: `mapping::Leaf::Number`.

- [ ] **Step 1: Write the failing test**

In `proxy.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn a_number_carrying_personal_data_refuses_the_request() {
        // The leaf walk masks strings, so a card number written as a JSON
        // number would otherwise be forwarded untouched. It cannot be replaced
        // — a schema that declared a number may reject a string — so the
        // request is refused instead.
        let detector = detector_returning(json!([
            {"entity_type": "CREDIT_CARD", "start": 0, "end": 16}
        ]))
        .await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let body = json!({
            "model": "claude",
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "pay",
                 "input": {"card": 4111111111111111u64}}
            ]}]
        });
        let (status, _) = call(state, "/v1/messages", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
```

Match the status to whatever `ShapeError::Unsupported` already produces — read `ProxyError`'s response mapping and use that, rather than asserting a status this plan guessed at.

- [ ] **Step 2: Run it and watch it fail**

Run: `cd gateway && cargo test --quiet a_number_carrying_personal_data`
Expected: FAIL — the request is served with the number forwarded.

- [ ] **Step 3: Refuse on a numeric hit**

In `mask_all`'s `Json` arm, replace the empty `Leaf::Number(_) => {}`:

```rust
                        mapping::Leaf::Number(rendered) => {
                            // Rendered as the client wrote it and looked at,
                            // never replaced: `[CREDIT_CARD_1]` in a field a
                            // schema declared numeric is a type error the
                            // client sees and the model may imitate.
                            //
                            // Only as wide as the vocabulary. ENTITY_TYPES has
                            // no telephone entity, so a phone number as a JSON
                            // number is forwarded — the same gap it has in
                            // ordinary text, and detection-quality work rather
                            // than this slice's.
                            let spans = detector.detect(rendered, credential).await?;
                            if !spans.is_empty() {
                                return Err(ProxyError::NumericPersonalData);
                            }
                        }
```

Add `NumericPersonalData` to `ProxyError` beside `ToolTooLarge`, mapped to the same status.

- [ ] **Step 4: Run everything**

Run: `cd gateway && cargo test --quiet && cargo fmt --check && touch src/*.rs && cargo clippy --all-targets -- -D warnings`

- [ ] **Step 5: Prove it by mutation**

1. Change the refusal to `if false`. Expected: the new test fails.
2. Add a second test in the same shape where the detector returns *no* spans for the number, and assert the request succeeds. Then mutate the refusal to `if true` and watch that second test fail. Without it, a implementation that refuses every numeric leaf would pass.

- [ ] **Step 6: Commit**

```bash
git add gateway/src/proxy.rs
git commit -m "fix(gateway): a number carrying personal data refuses the request

The leaf walk masks strings, so a card number written as a JSON number
was forwarded untouched. It cannot be masked without changing the leaf's
type, so it is refused instead — and the refusal is only as wide as the
vocabulary, which has no telephone entity."
```

---

### Task 7: The journal says the same for tool traffic

**Files:**
- Modify: `gateway/src/proxy.rs`, `README.md`

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn the_journal_counts_spans_found_in_tool_traffic() {
        // The evidence layer must not get weaker because the personal data
        // arrived in an argument rather than in a message.
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let body = json!({
            "model": "claude",
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "read_file",
                 "input": {"path": "Weber"}}
            ]}]
        });
        let (status, _) = call(state, "/v1/messages", body).await;
        assert_eq!(status, StatusCode::OK);

        let lines = journal(&path);
        let masked = lines.iter().find(|line| line["event"] == "masked").unwrap();
        assert_eq!(masked["types"]["PERSON"], 1);
        let text = serde_json::to_string(masked).unwrap();
        assert!(!text.contains("Weber"), "the journal never carries a value");
        assert!(!text.contains("path"), "the journal never carries a key name");
    }
```

- [ ] **Step 2: Run it**

Run: `cd gateway && cargo test --quiet the_journal_counts_spans_found_in_tool`
Expected: PASS if Task 4 wired `count_distinct` into the `Json` arm, FAIL otherwise. **If it passes immediately, prove it is not vacuous** by mutating the `Json` arm to skip `count_distinct` and watching it fail.

- [ ] **Step 3: Update the README**

`README.md` states what this gateway refuses. Find every sentence that says tool traffic is refused and correct it: tool definitions, arguments and results are masked on the buffered path; streamed tool traffic is still refused; extended thinking is still refused. Write in the README's existing voice, and state the `max_tool_bytes` ceiling where a reader meets the other limits, with issue #28 named as the work that lifts it.

- [ ] **Step 4: Full verification**

```bash
cd gateway && cargo test --quiet && cargo fmt --check && touch src/*.rs && cargo clippy --all-targets -- -D warnings
cd ../detector && uv run pytest -q && uv run ruff check . ../evaluation && uv run mypy src
cd .. && make check-entity-types && make check-layers && make check-base-install
```

- [ ] **Step 5: Commit**

```bash
git add gateway/src/proxy.rs README.md
git commit -m "test(gateway): the journal counts tool spans, and the README says so

Asserted rather than assumed: the evidence layer must not get weaker
because the personal data arrived in an argument rather than a message.
The README said tool traffic is refused, which is now true only of the
streamed half."
```

---

## Self-Review

**Spec coverage.** Every section maps to a task: the `Slot` kind to Task 1; the bounded walk and its depth and node limits to Task 2; `max_tool_bytes` and its three TOML files to Task 3; Anthropic's definitions, arguments and results, the whole-schema walk, the streamed-tool refusal and the size bound's wiring to Task 4; OpenAI's embedded arguments and `tool_call_id` to Task 5; the numeric-leaf refusal to Task 6; the journal and the README to Task 7. The spec's ten testing invariants map to tasks 2, 4, 5, 6 and 7.

**Out of scope, as the spec says:** streaming, extended thinking, making large results fast (issue #28), and images, which keep the policy they have.

**Type consistency.** `Slot` is defined in Task 1 and consumed unchanged by 4 and 5. `json_leaves` returns `Vec<Leaf>` in Task 2 and is destructured as `Leaf::Text`/`Leaf::Number` in Tasks 4 and 6. `replace_text_leaves` takes `&[String]` in Task 2 and is given `Vec<String>` by deref in Task 4. `read_document`/`write_document` are defined in Task 4 and reused by Task 5's response work. `max_tool_bytes` is `usize` in Task 3 and compared against a `usize` sum in Task 4. `ProxyError::ToolTooLarge` is added in Task 4 and `NumericPersonalData` beside it in Task 6.

**Known rough edges for implementers.** Task 4 is the largest and the only one that touches three concerns at once — it is not split because none of the three is independently testable: the refusal cannot be tested while tool fields are refused anyway, and the size bound has nothing to bound until a `Json` slot exists. Task 4's step 4 moves a `let mut pointers` declaration; move only that, and leave the `thinking` and `logprobs` checks where they are. Several tests name helpers that already exist in `proxy.rs` — read a neighbouring test and match its shape rather than trusting this plan's memory of their signatures.
