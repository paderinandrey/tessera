"""Command-line scan report: what would be redacted, per type (MVP Release 1 CLI).

Values are masked by default (first 4 + last 2 characters) so a report saved to
a file or CI log is not itself a PII leak; --show-values prints them verbatim.
"""

import json
from collections import Counter
from dataclasses import dataclass
from pathlib import Path

from .deterministic import DeterministicDetector
from .resolution import resolve

MASK_MIN_LENGTH = 8


def mask(value: str) -> str:
    if len(value) < MASK_MIN_LENGTH:
        return "…"
    return f"{value[:4]}…{value[-2:]}"


@dataclass(frozen=True, slots=True)
class Finding:
    entity_type: str
    start: int
    end: int
    confidence: float
    recognizer: str
    value: str


@dataclass(slots=True)
class FileReport:
    path: str
    findings: list[Finding]


@dataclass(slots=True)
class ScanReport:
    files: list[FileReport]
    skipped: list[str]


def scan(path: Path, detector: DeterministicDetector) -> ScanReport:
    files = [path] if path.is_file() else sorted(p for p in path.rglob("*") if p.is_file())
    report = ScanReport(files=[], skipped=[])
    for file in files:
        try:
            text = file.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            report.skipped.append(str(file))
            continue
        findings = [
            Finding(
                entity_type=span.entity_type,
                start=span.start,
                end=span.end,
                confidence=span.confidence,
                recognizer=span.recognizer,
                value=text[span.start : span.end],
            )
            for span in resolve(detector.detect(text)).spans
        ]
        report.files.append(FileReport(path=str(file), findings=findings))
    return report


def _type_counts(report: ScanReport) -> list[tuple[str, int]]:
    counts = Counter(f.entity_type for file in report.files for f in file.findings)
    return sorted(counts.items(), key=lambda item: (-item[1], item[0]))


def render_text(report: ScanReport, *, show_values: bool = False) -> str:
    lines: list[str] = []
    for file in report.files:
        if not file.findings:
            continue
        lines.append(file.path)
        for f in file.findings:
            value = f.value if show_values else mask(f.value)
            span_range = f"{f.start}–{f.end}"
            lines.append(f"  {f.entity_type:<12} {span_range:<7} {f.confidence:.2f}  {value}")
        lines.append("")
    total = sum(len(file.findings) for file in report.files)
    lines.append(f"Total: {len(report.files)} files, {total} findings")
    for entity_type, count in _type_counts(report):
        lines.append(f"  {entity_type:<12} {count}")
    if report.skipped:
        lines.append(f"Skipped: {len(report.skipped)} (not valid UTF-8)")
    return "\n".join(lines)


def render_json(report: ScanReport, *, show_values: bool = False) -> str:
    value_key = "value" if show_values else "masked_value"
    payload = {
        "files": [
            {
                "path": file.path,
                "findings": [
                    {
                        "type": f.entity_type,
                        "start": f.start,
                        "end": f.end,
                        "confidence": f.confidence,
                        "recognizer": f.recognizer,
                        value_key: f.value if show_values else mask(f.value),
                    }
                    for f in file.findings
                ],
            }
            for file in report.files
        ],
        "summary": {
            "files_scanned": len(report.files),
            "files_skipped": len(report.skipped),
            "total_findings": sum(len(file.findings) for file in report.files),
            "by_type": dict(_type_counts(report)),
        },
    }
    return json.dumps(payload, ensure_ascii=False, indent=2)
