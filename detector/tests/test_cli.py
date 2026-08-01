import json
import os
from pathlib import Path

import pytest

from tessera_detector.cli import (
    FileReport,
    Finding,
    ScanReport,
    main,
    mask,
    render_json,
    render_text,
    scan,
)
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


def _report() -> ScanReport:
    return ScanReport(
        files=[
            FileReport(
                path="letters/claim_07.txt",
                findings=[
                    Finding("FR_NIR", 12, 27, 0.98, "catalog:fr_nir", "185027512345678"),
                    Finding("IBAN", 103, 130, 1.0, "catalog:iban", "FR7630006000011234567890189"),
                ],
            ),
            FileReport(path="letters/empty.txt", findings=[]),
        ],
        skipped=[],
    )


def test_render_text_masks_values_and_summarizes() -> None:
    assert render_text(_report()) == (
        "letters/claim_07.txt\n"
        "  FR_NIR       12–27   0.98  1850…78\n"
        "  IBAN         103–130 1.00  FR76…89\n"
        "\n"
        "Total: 2 files, 2 findings\n"
        "  FR_NIR       1\n"
        "  IBAN         1"
    )


def test_render_text_show_values_prints_verbatim() -> None:
    assert "FR7630006000011234567890189" in render_text(_report(), show_values=True)


def test_render_text_sorts_summary_by_count_then_name() -> None:
    report = ScanReport(
        files=[
            FileReport(
                path="a.txt",
                findings=[
                    Finding("EMAIL", 0, 8, 0.95, "catalog:email", "a@bc.com"),
                    Finding("IBAN", 10, 37, 1.0, "catalog:iban", "FR7630006000011234567890189"),
                    Finding("IBAN", 40, 67, 1.0, "catalog:iban", "FR7630006000011234567890189"),
                ],
            )
        ],
        skipped=["a.bin"],
    )
    text = render_text(report)
    assert text.index("IBAN         2") < text.index("EMAIL        1")
    assert text.endswith("Skipped: 1 (not valid UTF-8)")


def test_render_text_empty_report() -> None:
    assert render_text(ScanReport(files=[], skipped=[])) == "Total: 0 files, 0 findings"


def test_render_json_shape_and_masking() -> None:
    payload = json.loads(render_json(_report()))
    assert payload["summary"] == {
        "files_scanned": 2,
        "files_skipped": 0,
        "files_unreadable": 0,
        "total_findings": 2,
        "by_type": {"FR_NIR": 1, "IBAN": 1},
    }
    finding = payload["files"][0]["findings"][0]
    assert finding == {
        "type": "FR_NIR",
        "start": 12,
        "end": 27,
        "confidence": 0.98,
        "recognizer": "catalog:fr_nir",
        "masked_value": "1850…78",
    }


def test_render_json_show_values_uses_value_key() -> None:
    finding = json.loads(render_json(_report(), show_values=True))["files"][0]["findings"][0]
    assert finding["value"] == "185027512345678"
    assert "masked_value" not in finding


def test_main_scan_prints_text_report(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    (tmp_path / "a.txt").write_text("mail: anna.keller@example.ch", encoding="utf-8")
    assert main(["scan", str(tmp_path)]) == 0
    captured = capsys.readouterr()
    assert "EMAIL" in captured.out
    assert "anna…ch" in captured.out
    assert "anna.keller@example.ch" not in captured.out


def test_main_scan_json_stdout_is_valid_json(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    (tmp_path / "a.txt").write_text("mail: anna.keller@example.ch", encoding="utf-8")
    (tmp_path / "b.bin").write_bytes(b"\xff\xfe")
    assert main(["scan", str(tmp_path), "--json"]) == 0
    captured = capsys.readouterr()
    payload = json.loads(captured.out)
    assert payload["summary"]["files_skipped"] == 1
    assert "b.bin" in captured.err


def test_main_missing_path_exits_2(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    assert main(["scan", str(tmp_path / "nope")]) == 2
    captured = capsys.readouterr()
    assert captured.out == ""
    assert "no such file or directory" in captured.err


def test_main_show_values_prints_verbatim(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    (tmp_path / "a.txt").write_text("mail: anna.keller@example.ch", encoding="utf-8")
    assert main(["scan", str(tmp_path), "--show-values"]) == 0
    assert "anna.keller@example.ch" in capsys.readouterr().out


def test_main_unreadable_path_exits_2(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    target = tmp_path / "locked.txt"
    target.write_text("mail: anna.keller@example.ch", encoding="utf-8")
    target.chmod(0o000)
    try:
        assert main(["scan", str(target)]) == 2
    finally:
        target.chmod(0o644)
    captured = capsys.readouterr()
    assert captured.out == ""
    assert "Permission denied" in captured.err


def test_main_unreadable_directory_exits_2(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    locked = tmp_path / "locked"
    locked.mkdir()
    (locked / "a.txt").write_text("mail: anna.keller@example.ch", encoding="utf-8")
    locked.chmod(0o000)
    try:
        assert main(["scan", str(locked)]) == 2
    finally:
        locked.chmod(0o755)
    captured = capsys.readouterr()
    assert captured.out == ""
    assert "Permission denied" in captured.err


def test_nested_unreadable_subdirectory_is_reported(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    (tmp_path / "a.txt").write_text("mail: anna.keller@example.ch", encoding="utf-8")
    locked = tmp_path / "locked"
    locked.mkdir()
    (locked / "secret.txt").write_text("mail: max.weber@example.de", encoding="utf-8")
    locked.chmod(0o000)
    try:
        assert main(["scan", str(tmp_path)]) == 0
    finally:
        locked.chmod(0o755)
    captured = capsys.readouterr()
    assert "EMAIL" in captured.out
    assert "Unreadable: 1" in captured.out
    assert str(locked) in captured.err


def test_render_text_reports_unreadable_entries() -> None:
    report = ScanReport(files=[], skipped=[], unreadable=["dir/locked"])
    assert render_text(report).endswith("Unreadable: 1")


def test_render_json_counts_unreadable_entries() -> None:
    report = ScanReport(files=[], skipped=[], unreadable=["dir/locked"])
    assert json.loads(render_json(report))["summary"]["files_unreadable"] == 1


def test_scan_does_not_follow_file_symlinks(tmp_path: Path) -> None:
    outside = tmp_path / "outside"
    outside.mkdir()
    (outside / "secret.txt").write_text("mail: anna.keller@example.ch", encoding="utf-8")
    root = tmp_path / "root"
    root.mkdir()
    (root / "link.txt").symlink_to(outside / "secret.txt")
    (root / "real.txt").write_text("mail: max.weber@example.de", encoding="utf-8")
    report = scan(root, DeterministicDetector())
    assert [f.path for f in report.files] == [str(root / "real.txt")]


def test_scan_ignores_special_files(tmp_path: Path) -> None:
    # A FIFO must never be opened: read blocks forever waiting for a writer.
    os.mkfifo(tmp_path / "pipe.fifo")
    (tmp_path / "a.txt").write_text("mail: anna.keller@example.ch", encoding="utf-8")
    report = scan(tmp_path, DeterministicDetector())
    assert [f.path for f in report.files] == [str(tmp_path / "a.txt")]
    assert report.skipped == []
    assert report.unreadable == []


def test_full_report_over_directory(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    fr = "Contact: anna.keller@example.ch pour le dossier."
    de = "Bitte an max.weber@example.de schreiben."
    (tmp_path / "a.txt").write_text(fr, encoding="utf-8")
    (tmp_path / "sub").mkdir()
    (tmp_path / "sub" / "b.txt").write_text(de, encoding="utf-8")
    (tmp_path / "zz.bin").write_bytes(b"\xff\xfe")
    assert main(["scan", str(tmp_path)]) == 0
    assert capsys.readouterr().out == (
        f"{tmp_path / 'a.txt'}\n"
        "  EMAIL        9–31    0.95  anna…ch\n"
        "\n"
        f"{tmp_path / 'sub' / 'b.txt'}\n"
        "  EMAIL        9–29    0.95  max.…de\n"
        "\n"
        "Total: 2 files, 2 findings\n"
        "  EMAIL        2\n"
        "Skipped: 1 (not valid UTF-8)\n"
    )
