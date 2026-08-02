"""Command-line scan report: what would be redacted, per type (MVP Release 1 CLI).

Values are masked by default (first 4 + last 2 characters) so a report saved to
a file or CI log is not itself a PII leak; --show-values prints them verbatim.
"""

import argparse
import json
import os
import stat as stat_module
import sys
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path

from .pipeline import Detector
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
    unreadable: list[str] = field(default_factory=list)


def _display_path(path: Path) -> str:
    # A filename with non-UTF-8 bytes reaches Python as lone surrogates, which
    # crash any later encode of the report; make it printable, marking the bytes.
    return str(path).encode("utf-8", "surrogateescape").decode("utf-8", "replace")


def scan(path: Path, detector: Detector) -> ScanReport:
    report = ScanReport(files=[], skipped=[])
    if path.is_file():
        # PATH itself must be readable: let the OSError reach main() -> exit 2.
        path.open("rb").close()
        files = [path]
    else:
        # os.walk suppresses nothing here: the root probe raises for an
        # unreadable PATH (exit 2), nested enumeration errors are reported.
        os.scandir(path).close()
        errors: list[OSError] = []
        found: list[Path] = []
        unreadable: list[str] = []
        for dirpath, _dirnames, filenames in os.walk(path, onerror=errors.append):
            for name in filenames:
                file = Path(dirpath) / name
                try:
                    # Explicit lstat: is_file() would silently swallow a failed
                    # stat (readable but unsearchable directory, mode r--).
                    mode = file.lstat().st_mode
                except OSError:
                    unreadable.append(_display_path(file))
                    continue
                # Physical regular files only: a FIFO would block open()
                # forever, a symlink could pull content from outside the root.
                if stat_module.S_ISREG(mode):
                    found.append(file)
        unreadable.extend(_display_path(Path(e.filename)) for e in errors if e.filename)
        report.unreadable.extend(sorted(unreadable))
        files = sorted(found)
    for file in files:
        try:
            # newline="" disables universal-newline translation: collapsing
            # \r\n would shift every offset out of original-text coordinates.
            text = file.read_text(encoding="utf-8", newline="")
        except UnicodeDecodeError:
            report.skipped.append(_display_path(file))
            continue
        except OSError:
            report.unreadable.append(_display_path(file))
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
        report.files.append(FileReport(path=_display_path(file), findings=findings))
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
    if report.unreadable:
        lines.append(f"Unreadable: {len(report.unreadable)}")
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
            "files_unreadable": len(report.unreadable),
            "total_findings": sum(len(file.findings) for file in report.files),
            "by_type": dict(_type_counts(report)),
        },
    }
    return json.dumps(payload, ensure_ascii=False, indent=2)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="tessera", description="PII detection toolkit")
    subparsers = parser.add_subparsers(dest="command", required=True)
    scan_parser = subparsers.add_parser("scan", help="report what would be redacted, per type")
    scan_parser.add_argument("path", type=Path, help="file or directory to scan")
    scan_parser.add_argument("--json", action="store_true", dest="as_json", help="JSON output")
    scan_parser.add_argument("--show-values", action="store_true", help="print values verbatim")
    args = parser.parse_args(argv)
    if not args.path.exists():
        print(f"tessera: {args.path}: no such file or directory", file=sys.stderr)
        return 2
    try:
        report = scan(args.path, Detector())
    except OSError as error:
        print(f"tessera: {args.path}: {error.strerror or error}", file=sys.stderr)
        return 2
    for skipped in report.skipped:
        print(f"tessera: skipped {skipped}: not valid UTF-8", file=sys.stderr)
    for unreadable in report.unreadable:
        print(f"tessera: skipped {unreadable}: permission denied", file=sys.stderr)
    render = render_json if args.as_json else render_text
    print(render(report, show_values=args.show_values))
    return 0
