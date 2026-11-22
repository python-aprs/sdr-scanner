import json

import numpy as np
import pytest
from scipy.io import wavfile

from synth import analysis, scenario, signals, writer
from synth.scenario import parse_freq


def make(**over):
    base = {
        "sample_rate": 250e3,
        "center_freq": "146.0 MHz",
        "duration": 1.0,
        "seed": 5,
        "noise_dbfs": -60,
        "start_time": "2026-11-22T06:54:03Z",
        "signals": [
            {
                "label": "a",
                "kind": "nbfm",
                "freq": "146.05 MHz",
                "start": 0.1,
                "duration": 0.8,
                "snr_db": 40,
                "audio": {"kind": "tone", "freq": 1000},
            }
        ],
    }
    base.update(over)
    return scenario.from_dict(base)


def collect(sc, block=1 << 20):
    return np.concatenate(list(signals.Generator(sc).blocks(block)))


def test_parse_freq():
    assert parse_freq("146.52 MHz") == 146.52e6
    assert parse_freq("-600 kHz") == -600e3
    assert parse_freq(8000) == 8000.0
    assert parse_freq("12.5khz") == 12.5e3
    with pytest.raises(ValueError):
        parse_freq("5 parsecs")


def test_validation_rejects_out_of_band():
    with pytest.raises(ValueError, match="outside"):
        make(signals=[{"kind": "nbfm", "freq": "147 MHz", "start": 0, "duration": 1}])
    with pytest.raises(ValueError, match="unknown kind"):
        make(signals=[{"kind": "am", "freq": "146 MHz", "start": 0, "duration": 1}])


def test_block_size_does_not_change_output():
    sc = make()
    a = collect(sc, 1 << 20)
    b = collect(sc, 4093)  # odd block size crossing transmission edges
    assert a.shape == b.shape == (sc.num_samples,)
    np.testing.assert_array_equal(a, b)


def test_seed_is_deterministic_and_matters():
    a = collect(make(seed=1))
    b = collect(make(seed=1))
    c = collect(make(seed=2))
    np.testing.assert_array_equal(a, b)
    assert not np.array_equal(a, c)


def test_tone_demodulates_to_right_frequency_and_deviation():
    sc = make(noise_dbfs=-90)
    x = collect(sc)
    s = sc.signals[0]
    seg = x[int(0.2 * sc.sample_rate) : int(0.8 * sc.sample_rate)]
    audio = analysis.nbfm_demod(seg, sc.sample_rate, s.freq - sc.center_freq, deviation=s.deviation)
    audio = audio[400:]  # skip filter start-up
    assert analysis.dominant_freq(audio, 8000) == pytest.approx(1000, abs=2)
    # Full deviation tone -> peak audio about 1.0 (sine amplitude).
    rms = np.sqrt(np.mean(audio**2))
    assert rms == pytest.approx(1 / np.sqrt(2), rel=0.05)
    assert analysis.tone_sinad(audio, 8000, 1000) > 30, analysis.tone_sinad(audio, 8000, 1000)


def test_snr_calibration():
    """Carrier power in snr_bandwidth relative to noise in the same bandwidth."""
    sc = make(
        noise_dbfs=-40,
        signals=[{"kind": "carrier", "freq": "146.05 MHz", "start": 0, "duration": 1, "snr_db": 20}],
    )
    x = collect(sc)
    noise_only = collect(make(noise_dbfs=-40, signals=[]))
    p_total = np.mean(np.abs(x) ** 2)
    p_noise = np.mean(np.abs(noise_only) ** 2)
    assert 10 * np.log10(p_noise) == pytest.approx(-40, abs=0.1)
    p_carrier = p_total - p_noise
    noise_in_bw = p_noise * 16e3 / sc.sample_rate
    assert 10 * np.log10(p_carrier / noise_in_bw) == pytest.approx(20, abs=0.3)


def test_signal_is_silent_outside_its_window():
    sc = make(
        noise_dbfs=-200,
        signals=[{"kind": "nbfm", "freq": "146.05 MHz", "start": 0.1, "duration": 0.8, "level_dbfs": -20}],
    )
    x = collect(sc)
    fs = sc.sample_rate
    assert np.max(np.abs(x[: int(0.099 * fs)])) < 1e-6
    assert np.max(np.abs(x[int(0.901 * fs) :])) < 1e-6
    assert np.abs(x[int(0.5 * fs)]) == pytest.approx(sc.amplitude(sc.signals[0]), rel=1e-6)


def test_carrier_offset_lands_on_right_bin():
    sc = make(
        noise_dbfs=-80,
        signals=[{"kind": "carrier", "freq": "146.0371 MHz", "start": 0, "duration": 1, "level_dbfs": -10}],
    )
    x = collect(sc)
    spec = np.abs(np.fft.fftshift(np.fft.fft(x * np.hanning(len(x)))))
    f = (np.argmax(spec) - len(x) / 2) * sc.sample_rate / len(x)
    assert f == pytest.approx(37.1e3, abs=2)


def test_speechlike_audio_round_trips():
    sc = make(
        noise_dbfs=-80,
        signals=[
            {"kind": "nbfm", "freq": "145.95 MHz", "start": 0, "duration": 1, "snr_db": 40, "audio": "speechlike"}
        ],
    )
    gen = signals.Generator(sc)
    x = np.concatenate(list(gen.blocks()))
    truth = gen.prepared[0].audio
    from scipy import signal as sps

    truth8k = sps.resample_poly(truth, 1, 6)[:8000]
    audio = analysis.nbfm_demod(x, sc.sample_rate, -50e3)
    _lag, r, t = analysis.align(truth8k, audio, max_lag=200)
    assert analysis.snr_vs_reference(r[500:], t[500:]) > 15


def test_quantize_ci16_bits_and_clipping():
    x = np.array([0.5 + 0.25j, 1.2 - 2.0j, 1e-4 + 0j])
    q, clipped = signals.quantize_ci16(x, 1.0, 12)
    assert clipped == 2
    assert q.dtype == np.dtype("<i2")
    assert q[0] % 16 == 0 and q[1] % 16 == 0  # 12-bit steps of 16 LSBs
    assert q[2] == 32767 or q[2] == 32752  # clipped to max (rounded to step, then clipped)
    assert q[3] == -32768
    assert q[4] == 0  # below one 12-bit step


def test_writer_outputs_sigmf_and_truth(tmp_path):
    sc = make(
        duration=0.5,
        signals=[
            {
                "label": "t1",
                "kind": "nbfm",
                "freq": "146.05 MHz",
                "start": 0.1,
                "duration": 0.3,
                "snr_db": 30,
                "audio": {"kind": "tone", "freq": 1000},
                "ctcss": 100.0,
            },
        ],
    )
    res = writer.write(sc, tmp_path / "cap")
    meta = json.loads(res.meta_path.read_text())
    assert meta["global"]["core:datatype"] == "ci16_le"
    assert meta["global"]["core:sample_rate"] == 250e3
    assert meta["captures"][0]["core:frequency"] == 146e6
    ann = meta["annotations"][0]
    assert ann["core:label"] == "t1"
    assert ann["core:sample_start"] == 25_000
    assert ann["core:sample_count"] == 75_000
    assert res.data_path.stat().st_size == 4 * sc.num_samples

    truth = json.loads(res.truth_path.read_text())
    tx = truth["transmissions"][0]
    assert tx["freq_hz"] == 146.05e6
    assert tx["snr_db"] == pytest.approx(30)
    assert tx["ctcss_hz"] == 100.0
    rate, pcm = wavfile.read(tmp_path / tx["audio_file"])
    assert rate == 8000 and len(pcm) == 2400

    x, _ = analysis.read_sigmf(tmp_path / "cap")
    assert len(x) == sc.num_samples


def test_cf32_format(tmp_path):
    sc = make(format="cf32", duration=0.1, signals=[])
    res = writer.write(sc, tmp_path / "f")
    assert res.data_path.stat().st_size == 8 * sc.num_samples
    x, meta = analysis.read_sigmf(tmp_path / "f")
    assert meta["global"]["core:datatype"] == "cf32_le"
    assert 10 * np.log10(np.mean(np.abs(x) ** 2)) == pytest.approx(-60, abs=0.2)


def test_bundled_scenarios_load():
    from pathlib import Path

    for p in sorted((Path(__file__).parent.parent / "scenarios").glob("*.yaml")):
        sc = scenario.load(p)
        assert sc.signals, p
