# Latency

The README carries a [summary table](../README.md#how-fast-it-is). This is the full
per-layer measurement, where the budget goes, and what every route to the target has cost
so far.

Measured on an Apple M3 Pro (11 cores, CPU only), with the pinned fp32 ONNX weights, over
text concatenated deterministically from the public corpus:

| Size | Deterministic | Chunk + tokenize | NER tier 2 | NER tier 3 | Total (median) | Total (p95) |
|---|---|---|---|---|---|---|
| sentence (80 chars) | 0.0 ms | 0.0 ms | 42 ms | 66 ms | 109 ms | 116 ms |
| paragraph (1 200 chars, one chunk) | 0.6 ms | 0.3 ms | 467 ms | 491 ms | 950 ms | 1 108 ms |
| document (6 000 chars, several chunks) | 3.3 ms | 1.9 ms | 2 556 ms | 3 180 ms | 5 524 ms | 6 086 ms |

Per-layer figures are medians and account for the total: preprocessing is shared across the
inference passes and timed once, so the parts sum to within a few percent of the whole. The
p95 column is the number REQ-38 asks about; on a developer machine under load it carries
contention as well as detector behaviour — the document row has measured between 4 854 ms
and 9 008 ms p95 across runs while its median moved under 10%. Treat the medians as the
stable signal and p95 as an upper bound until these run on dedicated hardware.

> REQ-38 targets p95 under 80 ms without the LLM layer, and the detector does not meet it
> with the NER layer enabled — not by a margin that tuning closes. The deterministic layer
> is effectively free at every size; the entire budget goes to the model, and it costs
> roughly a second per 1 200 characters on this CPU. Measuring one-sentence documents alone
> would have reported 116 ms and hidden that, which is why the harness uses a size ladder.
>
> The split says two useful things. Chunking and tokenization are free — 1.9 ms on a 6 000
> character document — so the cost is inference and nothing else. And three quasi-identifier
> labels cost about as much as eleven Article 9 labels: the price is paid per inference pass,
> not per label, so adding categories to an existing tier is nearly free while adding a tier
> is not.
>
> Every route to the target measured so far costs something. Collapsing the two inference
> passes into one comes in faster but loses the Article 9 spans the split exists to protect.
> Running the passes on two threads gains only 16%, because onnxruntime already saturates
> the cores and the passes compete rather than overlap. **The quantised graphs are measured
> below, and neither is a route.**

## The quantised graphs

The mirror ships `onnx/model_fp16.onnx` and `onnx/model_int8.onnx` beside the fp32 graph this
gateway loads. Both were run end to end — `benchmark.py --runs 8 --warmup 2` and
`evaluate.py --require-ner` — against an fp32 baseline taken in the same session, so the three
columns are comparable to each other even where they differ from the table above.

| size | fp32 | fp16 | int8 | fp16 vs fp32 | int8 vs fp32 |
|---|---|---|---|---|---|
| sentence (80 chars) | 115.4 ms | 159.0 ms | 41.1 ms | 0.73x | **2.81x** |
| paragraph (1 200) | 715.6 ms | 890.7 ms | 419.0 ms | 0.80x | **1.71x** |
| document (6 000) | 4 139.1 ms | 4 612.2 ms | 2 126.3 ms | 0.90x | **1.95x** |

**fp16 is slower than fp32 here.** onnxruntime's CPU execution provider has no native fp16
kernels, so it up-converts at runtime and pays the conversion — fp16 is a GPU optimisation.
That explanation is the standard one rather than something measured here, but the direction
holds across all three sizes and both statistics, so fp16 is not a candidate on this hardware
whatever the cause. Its quality is indistinguishable from fp32: identical PERSON
(0.985/0.882/0.931), identical Article 9 coverage (0.9783), identical 11 occurrences of 8
annotated entities reaching the provider, and one fewer ORG false positive.

**int8 is the only thing that has ever been faster, and it does not survive the gates.**
`evaluate.py --require-ner` exits 1 on it. Not degradation:

| | fp32 | int8 |
|---|---|---|
| PERSON recall | 0.882 | 0.013 (1 of 76) |
| LOCATION / ORG recall | 1.000 / 0.333 | 0 / 0 |
| all eleven Article 9 categories | working | all zero |
| Article 9 coverage | 0.9783 | 0.0217 |
| annotated entities reaching the provider | 11 occurrences of 8 | 140 occurrences of 123 |

This paragraph used to say int8 "halves every confidence score (`Diabetes` 0.948 → 0.476,
`IG Metall` 0.98 → 0.54), preserving ranking while invalidating every threshold calibrated
against fp32, so adopting it means recalibrating". The two scores are right and the conclusion
was too mild. Ranking may be preserved; the *detection outcome* is not, because a halved score
falls under the bar nearly everywhere. Recovering PERSON recall from 0.013 is a research task,
not a recalibration — and ORG precision is 0.154 on fp32, so there is very little room to lower
bars before precision goes with them.

**Which gate caught it is worth knowing.** Tier 1 recall stayed at 1.0000 on the int8 graph,
because Tier 1 is the deterministic layer and the model plays no part in it. The Article 9
coverage gate caught the swap, with three per-category and per-language failures before the
aggregate. A summary of these gates that names Tier 1 recall as the protection is naming the
one metric that is blind here.

What int8 would have bought, since nothing else has: a one-sentence p95 of 45.4 ms against
REQ-38's 80 ms target, which fp32 misses at 169.8 ms. That prize is behind the same wall.

**Three runs is not a measurement on this machine.** At `--runs 3 --warmup 1` — CI's smoke
settings — the sentence row reported int8 as 3.6x *slower*, reversed, because one discarded
run does not absorb onnxruntime session warm-up. The fp32 rows moved too. Every figure here is
`--runs 8 --warmup 2`.

Reproducing: `make model` passes `ignore_patterns=['onnx/model_*.onnx']`, so the quantised
graphs are not fetched by any Makefile target. Point `TESSERA_NER_MODEL` at a directory whose
`onnx/model.onnx` is the graph under test; a directory of symlinks does it without touching
the cache.
>
> The number is published here rather than gated in CI: timings on shared runners are noise,
> and a target that is not met should be visible rather than quietly enforced somewhere it
> never runs. CI runs the harness only to prove it still works.
