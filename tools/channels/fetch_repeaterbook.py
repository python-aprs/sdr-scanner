#!/usr/bin/env python3
"""Download RepeaterBook's export for some states, for import_repeaters.py.

    fetch_repeaterbook.py --state Washington --state Oregon -o repeaterbook/

writes `repeaterbook/<State>.json` (the API's JSON as returned). Files newer
than --max-age-hours are kept rather than fetched again, and requests are
spaced --delay seconds apart, as RepeaterBook asks of API clients. Based on
dzcb's repeaterbook module (github.com/mycodeplug/dzcb).

RepeaterBook may require an app token for its API; pass it with --token (or
RB_TOKEN in the environment) and it is sent as `Authorization: Bearer`.

Standard library only.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path

API = "https://www.repeaterbook.com/api/export.php"
USER_AGENT = "sdr-scanner repeater import (https://github.com/python-aprs/sdr-scanner)"


def fetch(state: str, out_dir: Path, token: str | None, max_age_s: float, last: list[float], delay: float) -> Path:
    path = out_dir / f"{state.replace(' ', '_')}.json"
    if path.exists() and path.stat().st_mtime > time.time() - max_age_s:
        print(f"{path}: cached", file=sys.stderr)
        return path
    wait = last[0] + delay - time.time()
    if wait > 0:
        print(f"waiting {wait:.0f} s between requests", file=sys.stderr)
        time.sleep(wait)
    url = f"{API}?{urllib.parse.urlencode({'state': state})}"
    headers = {"User-Agent": USER_AGENT}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=120) as resp:
        body = resp.read()
    last[0] = time.time()
    data = json.loads(body)  # fail before overwriting a good file with an error page
    if isinstance(data, dict) and "results" not in data:
        raise SystemExit(f"{state}: unexpected reply {body[:300]!r}")
    out_dir.mkdir(parents=True, exist_ok=True)
    path.write_bytes(body)
    n = len(data.get("results", []) if isinstance(data, dict) else data)
    print(f"{path}: {n} records", file=sys.stderr)
    return path


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--state", action="append", required=True, help="US state name, e.g. Oregon (repeatable)")
    ap.add_argument("-o", "--out", default="repeaterbook", help="folder for <State>.json")
    ap.add_argument("--token", default=os.environ.get("RB_TOKEN"), help="RepeaterBook API token (or RB_TOKEN)")
    ap.add_argument("--max-age-hours", type=float, default=12.0, help="reuse files newer than this")
    ap.add_argument("--delay", type=float, default=30.0, help="seconds between requests")
    args = ap.parse_args(argv)
    last = [0.0]
    for state in args.state:
        fetch(state, Path(args.out), args.token, args.max_age_hours * 3600, last, args.delay)
    return 0


if __name__ == "__main__":
    sys.exit(main())
