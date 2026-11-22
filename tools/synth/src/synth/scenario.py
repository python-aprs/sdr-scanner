"""Scenario files: a YAML description of a synthetic capture.

Example (see scenarios/ for more)::

    sample_rate: 2.5e6
    center_freq: 146.0 MHz
    duration: 3.0
    seed: 1
    format: ci16          # ci16 (default) or cf32
    adc_bits: 12          # ci16 only: quantise like a 12-bit ADC
    noise_dbfs: -50       # total complex noise power over the full bandwidth
    start_time: 2026-11-22T06:54:03Z
    signals:
      - kind: nbfm
        freq: 146.52 MHz
        start: 0.5
        duration: 1.5
        snr_db: 20        # carrier-to-noise in snr_bandwidth (default 16 kHz)
        deviation: 5 kHz
        audio: {kind: tone, freq: 1000}
        ctcss: 100.0

Frequencies accept unit strings ("146.52 MHz", "25 kHz") or bare Hz.
Levels: 0 dBFS is a complex sinusoid at full scale (amplitude 1.0).
"""

from __future__ import annotations

import math
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import yaml

_UNITS = {"": 1.0, "hz": 1.0, "khz": 1e3, "mhz": 1e6, "ghz": 1e9}


def parse_freq(v: Any) -> float:
    """Parse "146.52 MHz", "-600 kHz", 8000 or "8000" into Hz."""
    if isinstance(v, (int, float)):
        return float(v)
    m = re.fullmatch(r"\s*([-+]?[0-9_.eE+-]+)\s*([a-zA-Z]*)\s*", str(v))
    if not m:
        raise ValueError(f"invalid frequency {v!r}")
    num, unit = m.groups()
    try:
        mult = _UNITS[unit.lower()]
    except KeyError:
        raise ValueError(f"unknown frequency unit {unit!r} in {v!r}") from None
    return float(num.replace("_", "")) * mult


@dataclass
class Audio:
    kind: str = "tone"  # tone | speechlike | wav | silence
    freq: float = 1000.0  # tone
    path: str | None = None  # wav
    level: float = 1.0  # peak level relative to full deviation


@dataclass
class Signal:
    kind: str  # nbfm | carrier
    freq: float
    start: float
    duration: float
    snr_db: float | None = None
    level_dbfs: float | None = None
    snr_bandwidth: float = 16e3
    deviation: float = 5e3
    audio: Audio = field(default_factory=Audio)
    ctcss: float | None = None
    ctcss_fraction: float = 0.12
    preemphasis: bool = False
    freq_error: float = 0.0  # Hz, transmitter off-frequency
    drift: float = 0.0  # Hz per second
    ramp: float = 0.005  # s, raised-cosine key-up/key-down
    label: str | None = None

    @property
    def end(self) -> float:
        return self.start + self.duration


@dataclass
class Spur:
    freq: float
    level_dbfs: float


@dataclass
class Impairments:
    dc_offset: tuple[float, float] = (0.0, 0.0)
    iq_gain_db: float = 0.0
    iq_phase_deg: float = 0.0
    spurs: list[Spur] = field(default_factory=list)


@dataclass
class Scenario:
    sample_rate: float
    center_freq: float
    duration: float
    signals: list[Signal]
    seed: int = 0
    format: str = "ci16"
    adc_bits: int = 16
    full_scale: float = 1.0
    noise_dbfs: float = -60.0
    start_time: str = "2026-01-01T00:00:00Z"
    description: str = ""
    impairments: Impairments = field(default_factory=Impairments)
    base_dir: Path = field(default_factory=Path.cwd)

    @property
    def num_samples(self) -> int:
        return round(self.duration * self.sample_rate)

    @property
    def noise_power(self) -> float:
        return 10 ** (self.noise_dbfs / 10)

    def amplitude(self, s: Signal) -> float:
        """Carrier amplitude of a signal, from level_dbfs or snr_db."""
        if s.level_dbfs is not None:
            return 10 ** (s.level_dbfs / 20)
        snr = 20.0 if s.snr_db is None else s.snr_db
        noise_in_bw = self.noise_power * s.snr_bandwidth / self.sample_rate
        return math.sqrt(noise_in_bw * 10 ** (snr / 10))

    def snr_db(self, s: Signal) -> float:
        noise_in_bw = self.noise_power * s.snr_bandwidth / self.sample_rate
        return 10 * math.log10(self.amplitude(s) ** 2 / noise_in_bw)

    def validate(self) -> None:
        if self.sample_rate <= 0 or self.duration <= 0:
            raise ValueError("sample_rate and duration must be positive")
        if self.format not in ("ci16", "cf32"):
            raise ValueError(f"format must be ci16 or cf32, not {self.format!r}")
        if not 2 <= self.adc_bits <= 16:
            raise ValueError("adc_bits must be between 2 and 16")
        half = self.sample_rate / 2
        for i, s in enumerate(self.signals):
            name = s.label or f"signal {i}"
            if s.kind not in ("nbfm", "carrier"):
                raise ValueError(f"{name}: unknown kind {s.kind!r}")
            off = s.freq - self.center_freq
            if abs(off) + s.deviation > half:
                raise ValueError(f"{name}: {s.freq} Hz is outside the captured bandwidth")
            if s.start < 0 or s.duration <= 0:
                raise ValueError(f"{name}: start must be >= 0 and duration > 0")
            if s.audio.kind not in ("tone", "speechlike", "wav", "silence"):
                raise ValueError(f"{name}: unknown audio kind {s.audio.kind!r}")
            if s.audio.kind == "wav" and not s.audio.path:
                raise ValueError(f"{name}: audio kind wav needs a path")


def _freq_keys(d: dict, keys: tuple[str, ...]) -> dict:
    d = dict(d)
    for k in keys:
        if k in d and d[k] is not None:
            d[k] = parse_freq(d[k])
    return d


def from_dict(d: dict, base_dir: Path | None = None) -> Scenario:
    d = _freq_keys(d, ("sample_rate", "center_freq"))
    signals = []
    for raw in d.pop("signals", []) or []:
        raw = _freq_keys(raw, ("freq", "deviation", "snr_bandwidth", "freq_error"))
        audio = raw.pop("audio", None) or {}
        if isinstance(audio, str):
            audio = {"kind": audio}
        audio = _freq_keys(audio, ("freq",))
        if raw.get("kind") == "carrier":
            audio.setdefault("kind", "silence")
        signals.append(Signal(audio=Audio(**audio), **raw))
    imp = d.pop("impairments", None) or {}
    spurs = [Spur(freq=parse_freq(s["freq"]), level_dbfs=s["level_dbfs"]) for s in imp.pop("spurs", [])]
    iq = imp.pop("iq_imbalance", None) or {}
    impairments = Impairments(
        dc_offset=tuple(imp.pop("dc_offset", (0.0, 0.0))),
        iq_gain_db=iq.get("gain_db", 0.0),
        iq_phase_deg=iq.get("phase_deg", 0.0),
        spurs=spurs,
    )
    if imp:
        raise ValueError(f"unknown impairments: {sorted(imp)}")
    if "start_time" in d and not isinstance(d["start_time"], str):
        # YAML parses unquoted timestamps into datetime objects.
        d["start_time"] = d["start_time"].strftime("%Y-%m-%dT%H:%M:%SZ")
    sc = Scenario(signals=signals, impairments=impairments, base_dir=base_dir or Path.cwd(), **d)
    sc.validate()
    return sc


def load(path: str | Path) -> Scenario:
    path = Path(path)
    with open(path) as f:
        return from_dict(yaml.safe_load(f), base_dir=path.parent)
