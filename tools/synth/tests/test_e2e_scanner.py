"""End-to-end: Python writes a capture, the Rust `scanner peaks` finds every
transmission at the right frequency. Runs when SCANNER_BIN points at the
binary (CI sets it); skipped otherwise."""

import json
import os
import subprocess
from pathlib import Path

import pytest

from synth import scenario, writer

SCANNER = os.environ.get("SCANNER_BIN")
pytestmark = pytest.mark.skipif(not SCANNER, reason="SCANNER_BIN not set")


def test_scanner_finds_ci_smoke_transmissions(tmp_path):
    sc = scenario.load(Path(__file__).parent.parent / "scenarios" / "ci_smoke.yaml")
    res = writer.write(sc, tmp_path / "ci_smoke")
    truth = json.loads(res.truth_path.read_text())
    out = subprocess.run(
        [SCANNER, "peaks", str(tmp_path / "ci_smoke"), "--json", "--threshold", "10"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    peaks = json.loads(out)
    assert len(peaks) == len(truth["transmissions"]), peaks
    for tx in truth["transmissions"]:
        expected = tx["freq_hz"] + tx["freq_error_hz"]
        nearest = min(peaks, key=lambda p: abs(p["freq_hz"] - expected))
        assert abs(nearest["freq_hz"] - expected) < 1500, (tx["id"], nearest)


def test_scanner_info_reads_metadata(tmp_path):
    sc = scenario.load(Path(__file__).parent.parent / "scenarios" / "ci_smoke.yaml")
    sc.duration = 0.2
    writer.write(sc, tmp_path / "short")
    out = subprocess.run(
        [SCANNER, "info", str(tmp_path / "short.sigmf-meta")], check=True, capture_output=True, text=True
    ).stdout
    assert "146.0000 MHz" in out
    assert "500000 (0.200 s)" in out
    assert "ci16_le" in out
