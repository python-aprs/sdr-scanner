"""Score a `scanner run --json` result against a scenario's ground truth
(PLAN.md 4.3): detection recall and precision, timing, frequency, and audio
SNR of each recording against the truth audio."""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path

import numpy as np
from scipy import signal as sps
from scipy.io import wavfile

from .analysis import align, snr_vs_reference

# Must match the scanner's defaults (crates/core/src/scanner.rs).
DEEMPHASIS_TAU = 530e-6
HIGHPASS_HZ = 300.0
LOWPASS_HZ = 3400.0
FREQ_TOL_HZ = 3000.0


@dataclass
class Match:
    truth_id: str
    freq_hz: float
    truth_start: float
    truth_end: float
    found_freq_hz: float | None = None
    start_err: float | None = None  # scanner signal start - truth start (s)
    end_err: float | None = None
    audio_snr_db: float | None = None


@dataclass
class Score:
    matches: list[Match] = field(default_factory=list)
    false_positives: list[dict] = field(default_factory=list)
    ignored: list[str] = field(default_factory=list)

    @property
    def found(self) -> int:
        return sum(m.found_freq_hz is not None for m in self.matches)

    @property
    def recall(self) -> float:
        return self.found / max(len(self.matches), 1)


def _read_wav(path: Path) -> tuple[int, np.ndarray]:
    rate, x = wavfile.read(path)
    if x.dtype == np.int16:
        x = x.astype(np.float64) / 32768.0
    return rate, np.asarray(x, dtype=np.float64)


def reference_audio(path: Path, rate: int, preemphasis: bool) -> np.ndarray:
    """Truth audio passed through the receive-side filters the scanner applies
    (de-emphasis unless the transmitter pre-emphasised, CTCSS high-pass, audio
    low-pass), so the comparison measures noise and distortion only."""
    r, x = _read_wav(path)
    if r != rate:
        x = sps.resample_poly(x, rate, r)
    if not preemphasis:
        b, a = sps.bilinear([1.0], [DEEMPHASIS_TAU, 1.0], fs=rate)
        x = sps.lfilter(b, a, x)
    x = sps.sosfilt(sps.butter(4, HIGHPASS_HZ, "highpass", fs=rate, output="sos"), x)
    return sps.sosfilt(sps.butter(8, LOWPASS_HZ, "lowpass", fs=rate, output="sos"), x)


def score(truth_path: Path, result: dict, min_len: float = 0.3) -> Score:
    truth = json.loads(Path(truth_path).read_text())
    base = Path(truth_path).parent
    txs = result["transmissions"]
    used: set[int] = set()
    sc = Score()
    for t in truth["transmissions"]:
        if t["kind"] != "nbfm":
            sc.ignored.append(f"{t['id']} ({t['kind']})")
            continue
        if t["end_s"] - t["start_s"] < min_len:
            sc.ignored.append(f"{t['id']} (shorter than {min_len} s)")
            continue
        f = t["freq_hz"] + t.get("freq_error_hz", 0.0)
        m = Match(t["id"], f, t["start_s"], t["end_s"])
        sc.matches.append(m)
        cands = [
            (i, x)
            for i, x in enumerate(txs)
            if i not in used
            and abs(x["freq_hz"] - f) <= FREQ_TOL_HZ
            and x["start_s"] < t["end_s"]
            and x["start_s"] + x["audio_s"] > t["start_s"]
        ]
        if not cands:
            continue
        i, x = min(cands, key=lambda c: abs(c[1]["start_s"] - t["start_s"]))
        used.add(i)
        m.found_freq_hz = x["freq_hz"]
        sig_end = x["start_s"] + x["audio_s"] - result.get("tail_s", 0.2)
        m.end_err = sig_end - t["end_s"]
        m.start_err = (sig_end - x["signal_s"]) - t["start_s"]
        audio_file = t.get("audio_file")
        if audio_file and Path(x["wav"]).exists():
            rate, test = _read_wav(Path(x["wav"]))
            ref = reference_audio(base / audio_file, rate, t.get("preemphasis", False))
            # Expected lag of the truth audio within the recording.
            lag0 = round((t["start_s"] - x["start_s"]) * rate)
            if lag0 >= 0:
                test_seg = test[lag0:]
                ref_seg = ref
            else:
                test_seg, ref_seg = test, ref[-lag0:]
            n = min(len(test_seg), len(ref_seg))
            # Skip the first and last 100 ms (key-up and key-down transients).
            guard = rate // 10
            if n > 4 * guard:
                # Align to 1/8 sample: at 8 kHz a half-sample error alone
                # caps a 1 kHz tone at about 8 dB.
                up = 8
                r8 = sps.resample_poly(ref_seg[guard : n - guard], up, 1)
                t8 = sps.resample_poly(test_seg[guard : n - guard], up, 1)
                _, r, s = align(r8, t8, up * rate // 50)
                m.audio_snr_db = float(snr_vs_reference(r, s))
    for i, x in enumerate(txs):
        if i not in used:
            sc.false_positives.append(x)
    return sc


def report(sc: Score) -> str:
    lines = [f"{'truth':16} {'MHz':>10} {'found':>10} {'start err':>9} {'end err':>8} {'audio SNR':>9}"]
    for m in sc.matches:
        if m.found_freq_hz is None:
            lines.append(f"{m.truth_id:16} {m.freq_hz / 1e6:10.4f} {'MISSED':>10}")
            continue
        snr = f"{m.audio_snr_db:9.1f}" if m.audio_snr_db is not None else f"{'-':>9}"
        lines.append(
            f"{m.truth_id:16} {m.freq_hz / 1e6:10.4f} {m.found_freq_hz / 1e6:10.4f} "
            f"{m.start_err:+9.3f} {m.end_err:+8.3f} {snr}"
        )
    for x in sc.false_positives:
        lines.append(f"{'(not in truth)':16} {'':10} {x['freq_hz'] / 1e6:10.4f}  at {x['start_s']:.2f} s")
    for i in sc.ignored:
        lines.append(f"ignored: {i}")
    lines.append(f"recall {sc.found}/{len(sc.matches)} ({sc.recall:.0%}), {len(sc.false_positives)} not in truth")
    return "\n".join(lines)
