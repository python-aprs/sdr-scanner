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


def _fcs(data: bytes) -> int:
    crc = 0xFFFF
    for b in data:
        crc ^= b
        for _ in range(8):
            crc = (crc >> 1) ^ 0x8408 if crc & 1 else crc >> 1
    return ~crc & 0xFFFF


def _ui_frame(src: str, dst: str, info: bytes) -> bytes:
    def addr(call: str, ssid: int, last: bool) -> list[int]:
        return [ord(c) << 1 for c in f"{call:<6}"] + [0x60 | ssid << 1 | int(last)]

    return bytes(addr(dst, 0, False) + addr(src, 9, False) + addr("WIDE2", 1, True) + [0x03, 0xF0]) + info


def _afsk(frame: bytes, rate: int = 48000):
    import numpy as np

    flag = [(0x7E >> i) & 1 for i in range(8)]
    bits, ones = flag * 40, 0
    for b in frame + _fcs(frame).to_bytes(2, "little"):
        for i in range(8):
            bit = (b >> i) & 1
            bits.append(bit)
            ones = ones + 1 if bit else 0
            if ones == 5:
                bits.append(0)
                ones = 0
    bits += flag * 3
    tone, out, ph = True, [], 0.0
    for k, b in enumerate(bits):
        if not b:
            tone = not tone
        while len(out) < round((k + 1) * rate / 1200):
            ph += 2 * np.pi * (1200 if tone else 2200) / rate
            out.append(0.8 * np.sin(ph))
    return np.array(out)


def test_scanner_decodes_aprs_packets(tmp_path):
    """Pre-emphasised AFSK1200 on 144.39 MHz comes out as packet text."""
    import sqlite3

    import numpy as np
    from scipy.io import wavfile

    infos = [b"!4559.61N/12302.48W-e2e", b">status from the synth", b":KF7HVM   :hello{1"]
    a = np.concatenate([np.concatenate([_afsk(_ui_frame("KF7HVM", "APRS", i)), np.zeros(4800)]) for i in infos])
    wavfile.write(tmp_path / "afsk.wav", 48000, (a * 29000).astype(np.int16))
    sc = scenario.load(Path(__file__).parent.parent / "scenarios" / "ci_smoke.yaml")
    sc.center_freq, sc.duration = 144.6e6, 3.0
    s = sc.signals[0]
    s.freq, s.start, s.duration, s.snr_db, s.ctcss = 144.39e6, 0.2, len(a) / 48000, 15, None
    s.audio.kind, s.audio.path, s.preemphasis, s.deviation = "wav", str(tmp_path / "afsk.wav"), True, 3500
    sc.signals = [s]
    writer.write(sc, tmp_path / "afsk")
    out = subprocess.run(
        [
            SCANNER,
            "run",
            str(tmp_path / "afsk.sigmf-meta"),
            "--config",
            str(REPO / "config" / "rainier.toml"),
            "--out",
            str(tmp_path / "out"),
            "--json",
        ],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    assert json.loads(out)["packets"] == 3
    db = sqlite3.connect(tmp_path / "out" / "index.sqlite")
    texts = [t for (t,) in db.execute("SELECT text FROM packets ORDER BY id")]
    assert texts == [f"KF7HVM-9>APRS,WIDE2-1:{i.decode()}" for i in infos], texts


def test_scanner_tags_dtmf_digits(tmp_path):
    """DTMF sent over pre-emphasised NBFM with CTCSS is tagged in the index;
    speech-like audio on another channel gets no digits."""
    import sqlite3

    sc = scenario.load(Path(__file__).parent.parent / "scenarios" / "ci_smoke.yaml")
    sc.duration = 4.0
    dt, voice = sc.signals[0], sc.signals[1]
    dt.start, dt.duration, dt.snr_db, dt.preemphasis = 0.2, 3.4, 20, True
    dt.audio.kind, dt.audio.digits, dt.audio.on, dt.audio.off = "dtmf", "*73 0#D", 0.07, 0.06
    dt.audio.level = 0.6
    voice.start, voice.duration = 0.3, 3.5
    sc.signals = [dt, voice]
    writer.write(sc, tmp_path / "dtmf")
    subprocess.run(
        [
            SCANNER,
            "run",
            str(tmp_path / "dtmf.sigmf-meta"),
            "--config",
            str(REPO / "config" / "example-2m.toml"),
            "--profile",
            "2m-25k",
            "--out",
            str(tmp_path / "out"),
            "--json",
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    db = sqlite3.connect(tmp_path / "out" / "index.sqlite")
    tags = {round(f / 1e3): (d, t) for f, d, t in db.execute("SELECT freq_hz, dtmf, tone FROM transmissions")}
    assert tags == {146520: ("*73 0#D", "100.0"), 146840: (None, None)}, tags

    # Rescanning the folder finds the same digits.
    db.execute("UPDATE transmissions SET dtmf = NULL")
    db.commit()
    out = subprocess.run([SCANNER, "dtmf", str(tmp_path / "out")], check=True, capture_output=True, text=True).stdout
    assert out.strip().endswith("\t*73 0#D"), out
    assert [d for (d,) in db.execute("SELECT dtmf FROM transmissions ORDER BY freq_hz")] == ["*73 0#D", None]
