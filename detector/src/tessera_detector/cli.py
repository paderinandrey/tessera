"""Command-line scan report: what would be redacted, per type (MVP Release 1 CLI).

Values are masked by default (first 4 + last 2 characters) so a report saved to
a file or CI log is not itself a PII leak; --show-values prints them verbatim.
"""

MASK_MIN_LENGTH = 8


def mask(value: str) -> str:
    if len(value) < MASK_MIN_LENGTH:
        return "…"
    return f"{value[:4]}…{value[-2:]}"
