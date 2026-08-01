# CLI Scan Report — Design

**Goal:** the Release 1 CLI from the MVP roadmap: point `tessera scan` at a folder
of texts (or a single file) and get a report of what would be redacted, broken down
by type. A dry-run reporting surface over the existing detector — no redaction, no
new detection logic.

**Traceability:** MVP roadmap Release 1 ("CLI: папка с текстами → отчёт"), REQ-6/48
(offsets reported in original-text coordinates), product principle that originals
never leak into logs (masked values by default).

## Decisions made during brainstorming

- Found values are **masked by default** (operator can see full values with
  `--show-values`). A report saved to a file or CI log must not itself become a
  PII leak.
- Output is a **human-readable text report** on stdout; `--json` switches to a
  machine-readable JSON document. Warnings always go to stderr so stdout stays
  valid JSON.
- stdlib `argparse` with a `scan` subcommand; no new dependencies. The subcommand
  leaves room for a future `serve` (Release 2 HTTP contract) without breaking the
  interface.

## Interface

```
uv run --project detector tessera scan PATH [--json] [--show-values]
```

- `PATH` — a directory (scanned recursively, files visited in sorted order for
  deterministic output) or a single file.
- Exit codes: `0` success, including zero findings; `2` PATH does not exist or is
  unreadable. Findings do not affect the exit code — this is a report, not a gate.

## Components

```
detector/src/tessera_detector/cli.py   argparse entry, scan command, masking, rendering
detector/pyproject.toml                [project.scripts] tessera = "tessera_detector.cli:main"
detector/tests/test_cli.py             unit + integration tests
```

## Pipeline

One `DeterministicDetector` per run. Per file: read as UTF-8 →
`detector.detect(text)` → `resolve(spans)` → resolved spans into the report.
The report shows post-resolution spans — what would actually be cut.

A file that fails UTF-8 decoding is skipped with a warning on stderr and counted
in the summary as skipped; it never aborts the run. Likewise an unreadable nested
file or subdirectory (permission denied) is reported — stderr warning, `Unreadable:`
line in the text summary, `files_unreadable` in the JSON summary — never silently
omitted: a partial scan that looks complete would defeat the report's purpose.
Directory traversal uses `os.walk` with an error callback because `rglob` swallows
enumeration errors. Exit 2 stays reserved for PATH itself being missing/unreadable.

## Masking

Default: first 4 and last 2 characters of the span value, middle replaced with
`…`. Values shorter than 8 characters are masked entirely (`…`). `--show-values`
prints values verbatim.

## Text report

Per-file block (path, then one line per finding: type, `start–end`, confidence,
masked value), then a summary: files scanned/skipped, total findings, findings per
type.

```
letters/claim_07.txt
  FR_NIR       12–27   0.98  1 85…42
  IBAN         103–130 1.00  FR76…41

Total: 2 files, 5 findings
  IBAN         3
  FR_NIR       1
  EMAIL        1
```

## JSON report (`--json`)

One JSON object on stdout:

```json
{
  "files": [
    {"path": "letters/claim_07.txt", "findings": [
      {"type": "FR_NIR", "start": 12, "end": 27, "confidence": 0.98,
       "recognizer": "catalog:fr_nir", "masked_value": "1 85…42"}
    ]}
  ],
  "summary": {"files_scanned": 2, "files_skipped": 0, "total_findings": 5,
              "by_type": {"IBAN": 3, "FR_NIR": 1, "EMAIL": 1}}
}
```

With `--show-values`, findings carry `value` instead of `masked_value`.

## Testing (TDD)

- Masking: long, short (< 8 chars), and non-ASCII values.
- Text rendering and JSON schema of the output.
- Directory walk: recursive, sorted, non-UTF-8 file skipped and counted.
- Single-file PATH; empty directory produces a valid zero report.
- Exit codes: 0 with and without findings, 2 for a missing path.
- Integration: run over a temp directory with fixture texts derived from corpus
  templates; assert full stdout for both formats.

## Out of scope

Redaction/rewriting of files, NER layer, config files, `--fail-on-findings`
gating, glob filters. Add when a real need shows up.
