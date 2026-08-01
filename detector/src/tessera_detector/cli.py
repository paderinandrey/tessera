"""Command-line scan report: what would be redacted, per type (MVP Release 1 CLI).

Values are masked by default (first 4 + last 2 characters) so a report saved to
a file or CI log is not itself a PII leak; --show-values prints them verbatim.
"""

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
