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


REPO = Path(__file__).resolve().parents[3]


def test_scanner_run_records_ci_smoke(tmp_path):
    """`scanner run` writes one WAV per transmission at the right frequency and
    time, with audio close to the truth (PLAN.md 4.3)."""
    from synth import score

    sc = scenario.load(Path(__file__).parent.parent / "scenarios" / "ci_smoke.yaml")
    res = writer.write(sc, tmp_path / "ci_smoke")
    out = subprocess.run(
        [
            SCANNER,
            "run",
            str(tmp_path / "ci_smoke.sigmf-meta"),
            "--config",
            str(REPO / "config" / "example-2m.toml"),
            "--profile",
            "2m-25k",
            "--out",
            str(tmp_path / "out"),
            "--hang",
            "0.5",
            "--json",
        ],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    result = json.loads(out)
    s = score.score(res.truth_path, result)
    print(score.report(s))
    assert s.recall == 1.0
    assert not s.false_positives
    for m in s.matches:
        assert abs(m.start_err) < 0.05, m
        assert abs(m.end_err) < 0.05, m
        assert abs(m.found_freq_hz - m.freq_hz) < 1500, m
        # Python reference demod gets 29, 15 and 28 dB on these.
        assert m.audio_snr_db is not None and m.audio_snr_db > 12, m
    assert (tmp_path / "out" / "index.sqlite").exists()
    # The CTCSS tone of the simplex transmission is tagged, and only that.
    import sqlite3

    db = sqlite3.connect(tmp_path / "out" / "index.sqlite")
    tones = {round(f / 1e3): t for f, t in db.execute("SELECT freq_hz, tone FROM transmissions")}
    assert tones == {146520: "100.0", 146840: None, 145612: None}, tones
