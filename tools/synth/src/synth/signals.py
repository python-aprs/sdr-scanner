"""Signal synthesis: modulating audio, FM phase, and block-wise IQ generation.

Generation is block-wise so long, wide captures (10 MS/s for minutes) never
need the whole recording in memory, and the output is identical whatever block
size is used (tests check this).
"""

from __future__ import annotations

from collections.abc import Iterator
from dataclasses import dataclass

import numpy as np
from scipy import signal as sps
from scipy.io import wavfile

from .scenario import Scenario, Signal

AUDIO_RATE = 48_000  # internal modulating-audio rate


def speechlike(n: int, rate: int, rng: np.random.Generator) -> np.ndarray:
    """Band-limited noise with a syllabic envelope: a stand-in for speech that
    needs no audio files. Peak-normalised to about 1."""
    noise = rng.standard_normal(n)
    sos = sps.butter(4, [300, 3000], btype="band", fs=rate, output="sos")
    voiced = sps.sosfilt(sos, noise)
    # Syllables: ~4 Hz on/off gating, smoothed, with pauses.
    syl_rate = 4.0
    k = max(2, int(n / rate * syl_rate) + 2)
    gates = (rng.random(k) > 0.3).astype(float) * rng.uniform(0.4, 1.0, k)
    env = np.interp(np.arange(n) / rate * syl_rate, np.arange(k), gates)
    sos_env = sps.butter(2, 8, fs=rate, output="sos")
    env = np.clip(sps.sosfilt(sos_env, env), 0, None)
    x = voiced * env
    peak = np.percentile(np.abs(x), 99.9) or 1.0
    return np.clip(x / peak, -1, 1)


DTMF_ROWS = (697.0, 770.0, 852.0, 941.0)
DTMF_COLS = (1209.0, 1336.0, 1477.0, 1633.0)
DTMF_KEYS = ("123A", "456B", "789C", "*0#D")


def dtmf(digits: str, on: float, off: float, rate: int) -> np.ndarray:
    """Touch-tone digits, `on` seconds each with `off` seconds between; a
    space is a 1 s pause. Each tone at half amplitude, so the pair peaks at 1."""
    parts = []
    for d in digits.upper():
        if d == " ":
            parts.append(np.zeros(int(rate * 1.0)))
            continue
        r = next(i for i, row in enumerate(DTMF_KEYS) if d in row)
        c = DTMF_KEYS[r].index(d)
        t = np.arange(round(on * rate)) / rate
        parts.append(0.5 * (np.sin(2 * np.pi * DTMF_ROWS[r] * t) + np.sin(2 * np.pi * DTMF_COLS[c] * t)))
        parts.append(np.zeros(round(off * rate)))
    return np.concatenate(parts) if parts else np.zeros(0)


def preemphasize(x: np.ndarray, rate: int) -> np.ndarray:
    """6 dB/octave pre-emphasis above ~300 Hz, then limit and low-pass at 3 kHz,
    roughly what an NBFM transmitter's audio chain does."""
    tau = 530e-6
    # Bilinear transform of H(s) = 1 + s*tau, normalised to unity at 1 kHz.
    b, a = sps.bilinear([tau, 1.0], [1.0 / (2 * np.pi * 20e3), 1.0], fs=rate)
    y = sps.lfilter(b, a, x)
    _w, h = sps.freqz(b, a, worN=[1000.0], fs=rate)
    y = y / abs(h[0])
    y = np.clip(y, -1, 1)
    sos = sps.butter(4, 3000, fs=rate, output="sos")
    return sps.sosfilt(sos, y)


def modulating_audio(sc: Scenario, s: Signal, index: int) -> np.ndarray:
    """Audio in [-1, 1] at AUDIO_RATE for the whole transmission (before CTCSS)."""
    n = int(np.ceil(s.duration * AUDIO_RATE)) + 2
    t = np.arange(n) / AUDIO_RATE
    a = s.audio
    if s.kind == "carrier" or a.kind == "silence":
        x = np.zeros(n)
    elif a.kind == "tone":
        x = np.sin(2 * np.pi * a.freq * t)
    elif a.kind == "speechlike":
        rng = np.random.default_rng([sc.seed, 1000 + index])
        x = speechlike(n, AUDIO_RATE, rng)
    elif a.kind == "wav":
        path = sc.base_dir / a.path
        rate, data = wavfile.read(path)
        data = data.astype(np.float64)
        if data.ndim > 1:
            data = data.mean(axis=1)
        peak = np.max(np.abs(data)) or 1.0
        data = sps.resample_poly(data / peak, AUDIO_RATE, rate)
        reps = int(np.ceil(n / len(data)))
        x = np.tile(data, reps)[:n]
    elif a.kind == "dtmf":
        d = dtmf(a.digits, a.on, a.off, AUDIO_RATE)[:n]
        x = np.zeros(n)
        x[: len(d)] = d
    else:  # validated earlier
        raise ValueError(a.kind)
    x = x * a.level
    if s.preemphasis and s.kind == "nbfm":
        x = preemphasize(x, AUDIO_RATE)
    return x


@dataclass
class _Prepared:
    sig: Signal
    audio: np.ndarray  # truth audio (no CTCSS), AUDIO_RATE
    phase: np.ndarray  # cumulative modulation phase (rad) at AUDIO_RATE, from tx start
    amplitude: float
    first: int  # first sample index (inclusive)
    last: int  # last sample index (exclusive)


def _prepare(sc: Scenario, s: Signal, index: int) -> _Prepared:
    audio = modulating_audio(sc, s, index)
    n = len(audio)
    t = np.arange(n) / AUDIO_RATE
    mod = audio.copy()
    if s.ctcss and s.kind == "nbfm":
        mod = mod * (1 - s.ctcss_fraction) + s.ctcss_fraction * np.sin(2 * np.pi * s.ctcss * t)
    inst = (s.deviation * mod if s.kind == "nbfm" else 0.0) + s.freq_error + s.drift * t
    # Phase at the start of each audio sample: integral of instantaneous frequency.
    phase = np.concatenate(([0.0], np.cumsum(inst)[:-1])) * (2 * np.pi / AUDIO_RATE)
    fs = sc.sample_rate
    return _Prepared(
        sig=s,
        audio=audio,
        phase=phase,
        amplitude=sc.amplitude(s),
        first=round(s.start * fs),
        last=min(round(s.end * fs), sc.num_samples),
    )


class Generator:
    """Produces the scenario's IQ samples block by block (complex128, before
    quantisation), plus the truth audio of each transmission."""

    def __init__(self, sc: Scenario):
        self.sc = sc
        self.prepared = [_prepare(sc, s, i) for i, s in enumerate(sc.signals)]

    def blocks(self, block_size: int = 1 << 20) -> Iterator[np.ndarray]:
        sc = self.sc
        fs = sc.sample_rate
        noise_rng = np.random.default_rng([sc.seed, 0])
        noise_scale = np.sqrt(sc.noise_power / 2)
        imp = sc.impairments
        dc = complex(*imp.dc_offset)
        g = 10 ** (imp.iq_gain_db / 20)
        phi = np.deg2rad(imp.iq_phase_deg)
        total = sc.num_samples
        for n0 in range(0, total, block_size):
            n1 = min(n0 + block_size, total)
            m = n1 - n0
            idx = np.arange(n0, n1, dtype=np.int64)
            # Noise is drawn in one fixed-size stream regardless of block size:
            # interleaved (re, im) pairs from a single generator.
            z = noise_rng.standard_normal(2 * m).view(np.complex128) * noise_scale
            x = z
            for p in self.prepared:
                a, b = max(n0, p.first), min(n1, p.last)
                if a >= b:
                    continue
                sl = slice(a - n0, b - n0)
                tx_t = (idx[sl] - p.first) / fs  # seconds since key-up
                mod_phase = np.interp(tx_t * AUDIO_RATE, np.arange(len(p.phase)), p.phase)
                # Carrier offset phase, computed modulo one cycle for precision.
                df = p.sig.freq - sc.center_freq
                car = 2 * np.pi * np.mod(df * (idx[sl].astype(np.float64) / fs), 1.0)
                env = _ramp(tx_t, p.sig.duration, p.sig.ramp)
                x[sl] += p.amplitude * env * np.exp(1j * (car + mod_phase))
            for sp in imp.spurs:
                df = sp.freq - sc.center_freq
                ph = 2 * np.pi * np.mod(df * (idx / fs), 1.0)
                x = x + 10 ** (sp.level_dbfs / 20) * np.exp(1j * ph)
            if g != 1.0 or phi != 0.0:
                i, q = x.real, x.imag
                x = (i * g) + 1j * (q * np.cos(phi) - i * g * np.sin(phi))
            if dc:
                x = x + dc
            yield x


def _ramp(t: np.ndarray, duration: float, ramp: float) -> np.ndarray:
    if ramp <= 0:
        return np.ones_like(t)
    up = np.clip(t / ramp, 0, 1)
    down = np.clip((duration - t) / ramp, 0, 1)
    r = np.minimum(up, down)
    return 0.5 - 0.5 * np.cos(np.pi * r)


def quantize_ci16(x: np.ndarray, full_scale: float, bits: int) -> tuple[np.ndarray, int]:
    """Scale to int16 like an ADC with `bits` of resolution (MSB-aligned).
    Returns interleaved int16 I/Q and the number of clipped components."""
    scale = 32767.0 / full_scale
    step = 2 ** (16 - bits)
    iq = np.empty(2 * len(x), dtype=np.float64)
    iq[0::2] = x.real * scale
    iq[1::2] = x.imag * scale
    iq = np.round(iq / step) * step
    clipped = int(np.count_nonzero((iq > 32767) | (iq < -32768)))
    return np.clip(iq, -32768, 32767).astype("<i2"), clipped
