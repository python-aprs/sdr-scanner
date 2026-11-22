"""Write a scenario to disk: SigMF recording, ground truth JSON, truth audio."""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

import numpy as np
from scipy import signal as sps
from scipy.io import wavfile

from . import __version__
from .scenario import Scenario
from .signals import AUDIO_RATE, Generator, quantize_ci16

TRUTH_AUDIO_RATE = 8000


@dataclass
class Result:
    meta_path: Path
    data_path: Path
    truth_path: Path
    samples: int
    clipped: int


def write(sc: Scenario, out_base: str | Path, block_size: int = 1 << 20) -> Result:
    """Write `<out_base>.sigmf-data`, `.sigmf-meta`, `.truth.json` and
    `<out_base>.truth/<id>.wav` (8 kHz modulating audio per transmission)."""
    out_base = Path(out_base)
    out_base.parent.mkdir(parents=True, exist_ok=True)
    data_path = out_base.with_name(out_base.name + ".sigmf-data")
    meta_path = out_base.with_name(out_base.name + ".sigmf-meta")
    truth_path = out_base.with_name(out_base.name + ".truth.json")
    audio_dir = out_base.with_name(out_base.name + ".truth")
    audio_dir.mkdir(exist_ok=True)

    gen = Generator(sc)
    clipped = 0
    samples = 0
    with open(data_path, "wb") as f:
        for block in gen.blocks(block_size):
            if sc.format == "ci16":
                q, c = quantize_ci16(block, sc.full_scale, sc.adc_bits)
                clipped += c
                f.write(q.tobytes())
            else:
                f.write(block.astype("<c8").tobytes())
            samples += len(block)

    fs = sc.sample_rate
    transmissions = []
    annotations = []
    for i, p in enumerate(gen.prepared):
        s = p.sig
        tx_id = s.label or f"tx{i:03d}"
        audio_file = None
        if s.kind == "nbfm" and s.audio.kind != "silence":
            audio = sps.resample_poly(p.audio, TRUTH_AUDIO_RATE, AUDIO_RATE)
            n = round(s.duration * TRUTH_AUDIO_RATE)
            pcm = np.clip(audio[:n] * 32767, -32768, 32767).astype("<i2")
            audio_file = audio_dir / f"{tx_id}.wav"
            wavfile.write(audio_file, TRUTH_AUDIO_RATE, pcm)
        half_bw = s.deviation + 3000 if s.kind == "nbfm" else 500
        transmissions.append(
            {
                "id": tx_id,
                "kind": s.kind,
                "freq_hz": s.freq,
                "start_s": s.start,
                "end_s": min(s.end, sc.duration),
                "start_sample": p.first,
                "end_sample": p.last,
                "deviation_hz": s.deviation if s.kind == "nbfm" else 0.0,
                "amplitude_dbfs": 20 * np.log10(p.amplitude),
                "snr_db": sc.snr_db(s),
                "snr_bandwidth_hz": s.snr_bandwidth,
                "ctcss_hz": s.ctcss,
                "preemphasis": s.preemphasis,
                "freq_error_hz": s.freq_error,
                "drift_hz_per_s": s.drift,
                "audio_kind": s.audio.kind,
                "audio_file": str(audio_file.relative_to(out_base.parent)) if audio_file else None,
            }
        )
        annotations.append(
            {
                "core:sample_start": p.first,
                "core:sample_count": max(0, p.last - p.first),
                "core:freq_lower_edge": s.freq - half_bw,
                "core:freq_upper_edge": s.freq + half_bw,
                "core:label": tx_id,
                "core:comment": f"{s.kind} {s.audio.kind}, SNR {sc.snr_db(s):.1f} dB",
            }
        )
    annotations.sort(key=lambda a: a["core:sample_start"])

    meta = {
        "global": {
            "core:datatype": "ci16_le" if sc.format == "ci16" else "cf32_le",
            "core:sample_rate": fs,
            "core:version": "1.2.0",
            "core:description": sc.description or "sdr-scanner synthetic capture",
            "core:recorder": f"sdr-scanner-synth {__version__}",
            "synth:seed": sc.seed,
            "synth:noise_dbfs": sc.noise_dbfs,
            "synth:adc_bits": sc.adc_bits if sc.format == "ci16" else None,
        },
        "captures": [
            {
                "core:sample_start": 0,
                "core:frequency": sc.center_freq,
                "core:datetime": sc.start_time,
            }
        ],
        "annotations": annotations,
    }
    meta_path.write_text(json.dumps(meta, indent=2) + "\n")
    truth = {
        "sample_rate": fs,
        "center_freq": sc.center_freq,
        "duration_s": sc.duration,
        "samples": samples,
        "noise_dbfs": sc.noise_dbfs,
        "clipped_components": clipped,
        "transmissions": transmissions,
    }
    truth_path.write_text(json.dumps(truth, indent=2) + "\n")
    return Result(meta_path, data_path, truth_path, samples, clipped)
