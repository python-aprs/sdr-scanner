"""Command line: `synth generate scenario.yaml -o out/name`."""

from __future__ import annotations

import argparse
import sys
import time
from pathlib import Path

from . import scenario, writer


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="synth", description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)
    g = sub.add_parser("generate", help="write a SigMF recording and ground truth")
    g.add_argument("scenario", type=Path)
    g.add_argument("-o", "--out", type=Path, help="output base path (default: next to the scenario)")
    g.add_argument("--seed", type=int, help="override the scenario seed")
    g.add_argument("--duration", type=float, help="override the duration (s)")
    g.add_argument("--block", type=int, default=1 << 20, help="samples per block")
    d = sub.add_parser("describe", help="print a scenario's signals")
    d.add_argument("scenario", type=Path)
    s = sub.add_parser("score", help="score `scanner run --json` output against ground truth")
    s.add_argument("truth", type=Path, help="<name>.truth.json")
    s.add_argument("result", type=Path, help="JSON written by `scanner run --json`")
    s.add_argument("--min-len", type=float, default=0.3)
    args = ap.parse_args(argv)

    if args.cmd == "score":
        import json

        from . import score

        sc = score.score(args.truth, json.loads(args.result.read_text()), args.min_len)
        print(score.report(sc))
        return 0 if sc.recall == 1.0 else 1

    sc = scenario.load(args.scenario)
    if args.cmd == "describe":
        print(f"{sc.sample_rate / 1e6:g} MS/s at {sc.center_freq / 1e6:.4f} MHz, {sc.duration:g} s")
        for s in sc.signals:
            print(
                f"  {s.label or '-':12} {s.kind:7} {s.freq / 1e6:10.4f} MHz  "
                f"{s.start:7.2f}–{s.end:7.2f} s  SNR {sc.snr_db(s):5.1f} dB  {s.audio.kind}"
            )
        return 0

    if args.seed is not None:
        sc.seed = args.seed
    if args.duration is not None:
        sc.duration = args.duration
    out = args.out or args.scenario.with_suffix("")
    t = time.monotonic()
    res = writer.write(sc, out, block_size=args.block)
    dt = time.monotonic() - t
    print(
        f"wrote {res.data_path} ({res.samples} samples, {res.samples / sc.sample_rate:.2f} s) "
        f"in {dt:.1f} s; truth in {res.truth_path}",
        file=sys.stderr,
    )
    if res.clipped:
        print(f"warning: {res.clipped} I/Q components clipped", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
