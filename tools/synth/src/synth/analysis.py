"""Reference DSP in numpy/scipy, used to check generated captures and, later,
as the golden reference the Rust demodulator is compared against
(PLAN.md 4.3). Clarity over speed."""

from __future__ import annotations

import json
from pathlib import Path

import numpy as np
from scipy import signal as sps


def read_sigmf(base: str | Path) -> tuple[np.ndarray, dict]:
    """Read `<base>.sigmf-data` (ci16_le or cf32_le) as normalised complex64."""
    base = Path(str(base).removesuffix(".sigmf-data").removesuffix(".sigmf-meta"))
    meta = json.loads(base.with_name(base.name + ".sigmf-meta").read_text())
    dt = meta["global"]["core:datatype"]
    raw = base.with_name(base.name + ".sigmf-data")
    if dt == "ci16_le":
        iq = np.fromfile(raw, dtype="<i2").astype(np.float32) / 32768.0
        x = iq.view(np.complex64)
    elif dt == "cf32_le":
        x = np.fromfile(raw, dtype="<c8")
    else:
        raise ValueError(f"unsupported datatype {dt}")
    return x, meta


def nbfm_demod(
    iq: np.ndarray,
    fs: float,
    offset_hz: float,
    deviation: float = 5e3,
    channel_bw: float = 16e3,
    audio_rate: int = 8000,
    deemphasis: bool = False,
) -> np.ndarray:
    """Demodulate one NBFM channel at `offset_hz` from the capture centre.
    Returns audio at `audio_rate`, scaled so full deviation is +/-1."""
    n = np.arange(len(iq))
    bb = iq * np.exp(-2j * np.pi * offset_hz * n / fs)
    # Decimate to an intermediate rate that is a multiple of the audio rate.
    inter = audio_rate * 4  # 32 kS/s for 8 kHz audio
    g = np.gcd(round(fs), inter)
    up, down = inter // g, round(fs) // g
    # resample_poly applies its own anti-alias filter; then tighten to channel.
    bb = sps.resample_poly(bb, up, down)
    taps = sps.firwin(129, channel_bw / 2, fs=inter)
    bb = sps.lfilter(taps, 1.0, bb)
    disc = np.angle(bb[1:] * np.conj(bb[:-1])) * inter / (2 * np.pi)
    audio = disc / deviation
    if deemphasis:
        b, a = sps.bilinear([1.0], [530e-6, 1.0], fs=inter)
        audio = sps.lfilter(b, a, audio)
    # Linear-phase FIRs throughout, with delays that are whole samples at the
    # audio rate (129 taps: 64 samples, 257 taps: 128 samples at 4x audio rate),
    # so the output aligns with the truth audio by an integer lag.
    audio = sps.lfilter(sps.firwin(257, 3500, fs=inter), 1.0, audio)
    return sps.resample_poly(audio, 1, inter // audio_rate)


def align(ref: np.ndarray, test: np.ndarray, max_lag: int) -> tuple[int, np.ndarray, np.ndarray]:
    """Find the lag (test relative to ref) maximising cross-correlation within
    +/- max_lag samples; return (lag, ref_aligned, test_aligned)."""
    c = sps.correlate(test, ref, mode="full", method="fft")
    mid = len(ref) - 1
    lo, hi = max(0, mid - max_lag), min(len(c), mid + max_lag + 1)
    lag = int(np.argmax(c[lo:hi]) + lo - mid)
    if lag >= 0:
        t, r = test[lag:], ref
    else:
        t, r = test, ref[-lag:]
    n = min(len(t), len(r))
    return lag, r[:n], t[:n]


def snr_vs_reference(ref: np.ndarray, test: np.ndarray) -> float:
    """SNR (dB) of `test` against `ref` after least-squares gain matching."""
    g = np.dot(test, ref) / np.dot(ref, ref)
    err = test - g * ref
    return 10 * np.log10(np.dot(g * ref, g * ref) / max(np.dot(err, err), 1e-30))


def tone_sinad(audio: np.ndarray, rate: float, tone_hz: float) -> float:
    """SINAD (dB) of a single-tone audio signal: total power over power with
    the tone notched out."""
    x = audio - np.mean(audio)
    b, a = sps.iirnotch(tone_hz, Q=5, fs=rate)
    residual = sps.filtfilt(b, a, x)
    return 10 * np.log10(np.mean(x**2) / max(np.mean(residual**2), 1e-30))


def dominant_freq(audio: np.ndarray, rate: float) -> float:
    w = np.hanning(len(audio))
    spec = np.abs(np.fft.rfft((audio - audio.mean()) * w))
    k = int(np.argmax(spec[1:]) + 1)
    # Parabolic interpolation for sub-bin accuracy.
    if 1 <= k < len(spec) - 1:
        a, b, c = np.log(spec[k - 1 : k + 2] + 1e-30)
        k = k + 0.5 * (a - c) / (a - 2 * b + c)
    return k * rate / len(audio)
