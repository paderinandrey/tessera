# Security Policy

Tessera is a security product in the critical path of personal data. We take
vulnerability reports seriously and appreciate responsible disclosure.

## Reporting a vulnerability

Please **do not open a public issue** for security-sensitive reports.

- Use GitHub's [private vulnerability reporting](../../security/advisories/new) for this
  repository, or
- email the maintainer: andy.paderin@gmail.com

You will receive an acknowledgement within 72 hours. Please include reproduction steps
and an assessment of impact (e.g., data exposure, placeholder-restoration bypass,
mapping-table access).

## Scope of particular interest

- Original values leaking into logs, audit records, error messages, or traces
- Cross-session placeholder restoration (session isolation bypass)
- Fail-open behavior: raw data reaching the upstream provider on any internal failure
- Detection bypasses for Tier 1 identifiers (checksum-validated types, Art. 9 categories)

## Supported versions

Pre-1.0: only the latest release receives security fixes.
