from pathlib import Path

from tessera_detector.cli import mask, scan
from tessera_detector.deterministic import DeterministicDetector


def test_mask_keeps_first_four_and_last_two() -> None:
    assert mask("FR7630006000011234567890189") == "FR76…89"


def test_mask_hides_short_values_entirely() -> None:
    # 7 chars: revealing 6 of them would leave nothing masked.
    assert mask("a@b.com") == "…"


def test_mask_eight_chars_is_partial() -> None:
    assert mask("ab@cd.ef") == "ab@c…ef"


def test_mask_counts_characters_not_bytes() -> None:
    assert mask("héloïse@exämple.ch") == "hélo…ch"


def test_scan_reports_findings_with_values_from_text(tmp_path: Path) -> None:
    text = "Contact: anna.keller@example.ch pour le dossier."
    (tmp_path / "a.txt").write_text(text, encoding="utf-8")
    report = scan(tmp_path, DeterministicDetector())
    assert [f.path for f in report.files] == [str(tmp_path / "a.txt")]
    finding = report.files[0].findings[0]
    assert finding.entity_type == "EMAIL"
    assert (finding.start, finding.end) == (9, 31)
    assert finding.value == "anna.keller@example.ch"
    assert finding.recognizer == "catalog:email"


def test_scan_walks_directories_recursively_in_sorted_order(tmp_path: Path) -> None:
    (tmp_path / "sub").mkdir()
    text = "Bitte an max.weber@example.de schreiben."
    (tmp_path / "sub" / "b.txt").write_text(text, encoding="utf-8")
    (tmp_path / "a.txt").write_text("nothing personal here", encoding="utf-8")
    report = scan(tmp_path, DeterministicDetector())
    expected = [str(tmp_path / "a.txt"), str(tmp_path / "sub" / "b.txt")]
    assert [f.path for f in report.files] == expected
    assert report.files[0].findings == []
    assert report.files[1].findings[0].entity_type == "EMAIL"


def test_scan_skips_non_utf8_files_without_aborting(tmp_path: Path) -> None:
    (tmp_path / "c.bin").write_bytes(b"\xff\xfe\x00\x01")
    (tmp_path / "a.txt").write_text("mail: anna.keller@example.ch", encoding="utf-8")
    report = scan(tmp_path, DeterministicDetector())
    assert report.skipped == [str(tmp_path / "c.bin")]
    assert len(report.files) == 1


def test_scan_accepts_a_single_file(tmp_path: Path) -> None:
    target = tmp_path / "one.txt"
    target.write_text("mail: anna.keller@example.ch", encoding="utf-8")
    report = scan(target, DeterministicDetector())
    assert [f.path for f in report.files] == [str(target)]
    assert report.files[0].findings[0].entity_type == "EMAIL"
