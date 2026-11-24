#!/usr/bin/env python3
"""Turn repeater coordinator lists into sdr-scanner band-profile entries.

Supported inputs:
  * WWARA (Washington) CSV export: has coordinates and per-mode flags.
  * ORRC (Oregon) "Coordinations.xls" export: actually an HTML table; no
    coordinates or mode flags, so it is filtered by region and the mode/tone
    are parsed from the free-form ARRL_Code column.

Output is TOML `[[profiles.<name>.repeaters]]` entries (stdout or a file) that
`scanner config check` accepts. Each repeater creates an output slot and an
input slot (output + offset).

Example (Rainier, OR):
  import_repeaters.py --wwara WWARA-rptrlist.csv --orrc ORRC_Coordinations.xls \\
      --lat 46.089 --lon -122.936 --radius-km 100 --band 2m --profile 2m-rainier

Standard library only, so it runs anywhere Python 3.10+ does.
"""

from __future__ import annotations

import argparse
import csv
import html
import io
import math
import re
import sys
from dataclasses import dataclass, field

# Repeater sub-bands: 70 cm repeaters live in 440–450 MHz (below is links, ATV, satellites).
BANDS = {"2m": (144.0, 148.0), "1.25m": (222.0, 225.0), "70cm": (440.0, 450.0)}

# ORRC "ARRL_Region" values considered in range by default (Rainier, OR sits in
# the Lower Columbia / NW Oregon & SW Washington area). Matched case-insensitively.
DEFAULT_ORRC_AREAS = r"portland|lower columbia|nw oregon|sw washington|north willamette"


@dataclass
class Repeater:
    output_mhz: float
    input_mhz: float
    call: str
    location: str
    source: str
    tone: float | None = None
    dcs: str | None = None
    modes: list[str] = field(default_factory=list)  # e.g. ["FM"], ["FM", "DMR"], ["NFM"]
    distance_km: float | None = None
    notes: list[str] = field(default_factory=list)

    @property
    def offset_khz(self) -> float:
        return round((self.input_mhz - self.output_mhz) * 1000, 3)

    @property
    def has_fm(self) -> bool:
        return any(m in ("FM", "NFM") for m in self.modes) or not self.modes

    @property
    def primary_mode(self) -> str:
        """Scanner mode for the slot: FM wins when present (mixed-mode
        repeaters still carry analog), else the single digital mode."""
        if self.has_fm:
            return "nbfm"
        for m in ("DMR", "YSF", "DSTAR", "P25", "NXDN"):
            if m in self.modes:
                return m.lower()
        return "nbfm"

    @property
    def narrow(self) -> bool:
        return "NFM" in self.modes and "FM" not in self.modes


def haversine_km(lat1: float, lon1: float, lat2: float, lon2: float) -> float:
    r = math.radians
    a = math.sin(r(lat2 - lat1) / 2) ** 2 + math.cos(r(lat1)) * math.cos(r(lat2)) * math.sin(r(lon2 - lon1) / 2) ** 2
    return 2 * 6371.0 * math.asin(math.sqrt(a))


def _float(s: str) -> float | None:
    try:
        return float(s.strip())
    except (ValueError, AttributeError):
        return None


# ----------------------------------------------------------------------------- WWARA


def parse_wwara(text: str) -> list[Repeater]:
    lines = text.splitlines()
    if lines and lines[0].startswith("DATA_SPEC_VERSION"):
        lines = lines[1:]
    out = []
    for row in csv.DictReader(io.StringIO("\n".join(lines))):
        f_out, f_in = _float(row.get("OUTPUT_FREQ", "")), _float(row.get("INPUT_FREQ", ""))
        if f_out is None or f_in is None:
            continue

        def flag(k: str, row: dict = row) -> bool:
            return (row.get(k) or "").strip().upper() == "Y"

        modes = []
        if flag("FM_WIDE"):
            modes.append("FM")
        if flag("FM_NARROW"):
            modes.append("NFM")
        if flag("DMR"):
            modes.append("DMR")
        if flag("FUSION"):
            modes.append("YSF")
        if flag("DSTAR_DV"):
            modes.append("DSTAR")
        if flag("P25_PHASE_1") or flag("P25_PHASE_2"):
            modes.append("P25")
        if flag("NXDN_DIGITAL") or flag("NXDN_MIXED"):
            modes.append("NXDN")
        r = Repeater(
            output_mhz=f_out,
            input_mhz=f_in,
            call=row.get("CALL", "").strip(),
            location=f"{row.get('CITY', '').strip()}, {row.get('STATE', '').strip()}".strip(", "),
            source="WWARA",
            tone=_float(row.get("CTCSS_IN", "")),
            dcs=(row.get("DCS_CDCSS") or "").strip() or None,
            modes=modes,
        )
        lat, lon = _float(row.get("LATITUDE", "")), _float(row.get("LONGITUDE", ""))
        r._latlon = (lat, lon) if lat is not None and lon is not None else None  # type: ignore[attr-defined]
        if flag("DMR") and row.get("DMR_COLOR_CODE", "").strip():
            r.notes.append(f"DMR CC{row['DMR_COLOR_CODE'].strip()}")
        out.append(r)
    return out


# ----------------------------------------------------------------------------- ORRC

_TONE_RE = re.compile(r"(?<![\dDd])(\d{2,3}\.\d)(?!\d)")
_TONE_INT_RE = re.compile(r"t\s*\(\s*(\d{2,3})\s*\)", re.IGNORECASE)
_DCS_RE = re.compile(r"\b(?:DCS\s*|D)(\d{3})\b", re.IGNORECASE)


def parse_orrc_code(code: str) -> tuple[float | None, str | None, list[str]]:
    """Pull CTCSS tone, DCS code and digital modes out of ORRC's free-form
    ARRL_Code column, e.g. "oet(107.2)", "olt(D023)", "oe(dstar)", "ot(100)e"."""
    code = code or ""
    tone = None
    m = _TONE_RE.search(code)
    if m and 60.0 <= float(m.group(1)) <= 260.0:
        tone = float(m.group(1))
    else:
        m = _TONE_INT_RE.search(code)
        if m and 60 <= int(m.group(1)) <= 260:
            tone = float(m.group(1))
    d = _DCS_RE.search(code)
    dcs = d.group(1) if d else None
    lc = code.lower()
    modes = []
    if "dstar" in lc or "d-star" in lc:
        modes.append("DSTAR")
    if "dmr" in lc:
        modes.append("DMR")
    if "fusion" in lc or "ysf" in lc or "c4fm" in lc:
        modes.append("YSF")
    if "p25" in lc:
        modes.append("P25")
    if "nxdn" in lc:
        modes.append("NXDN")
    return tone, dcs, modes


def _orrc_location(city: str) -> str:
    city = city.strip()
    if not city:
        return ""
    m = re.fullmatch(r"(.*?)[ ,]+(WA|OR|ID|CA)", city, re.IGNORECASE)
    if m:
        return f"{m.group(1).strip()}, {m.group(2).upper()}"
    return f"{city}, OR"


def parse_orrc(text: str) -> list[tuple[Repeater, dict]]:
    rows = re.findall(r"<tr[^>]*>(.*?)</tr>", text, re.DOTALL | re.IGNORECASE)
    if not rows:
        raise ValueError("no table rows found; is this the ORRC Coordinations export?")
    hdr = [
        html.unescape(re.sub(r"<[^>]+>", "", h)).strip() for h in re.findall(r"<th[^>]*>(.*?)</th>", rows[0], re.DOTALL)
    ]
    out = []
    for r in rows[1:]:
        cells = [
            html.unescape(re.sub(r"<[^>]+>", "", c)).replace("\xa0", " ").strip()
            for c in re.findall(r"<td[^>]*>(.*?)</td>", r, re.DOTALL)
        ]
        rec = dict(zip(hdr, cells))
        f_out, f_in = _float(rec.get("TxFreq", "")), _float(rec.get("RxFreq", ""))
        if f_out is None or f_in is None:
            continue
        tone, dcs, digital = parse_orrc_code(rec.get("ARRL_Code", ""))
        rep = Repeater(
            output_mhz=f_out,
            input_mhz=f_in,
            call=rec.get("RepeaterCallSign", ""),
            location=_orrc_location(rec.get("NearestCity", "")),
            source="ORRC",
            tone=tone,
            dcs=dcs,
            # ORRC does not say whether analog is present; dstar-tagged entries
            # are treated as digital-only, everything else as FM.
            modes=digital or ["FM"],
        )
        out.append((rep, rec))
    return out


# ----------------------------------------------------------------------------- selection


def select(
    wwara: list[Repeater],
    orrc: list[tuple[Repeater, dict]],
    band: tuple[float, float],
    lat: float | None,
    lon: float | None,
    radius_km: float,
    orrc_regions: set[str],
    orrc_areas: str,
) -> tuple[list[Repeater], list[str]]:
    lo, hi = band
    log = []
    chosen = []
    for r in wwara:
        if not lo <= r.output_mhz <= hi:
            continue
        if abs(r.offset_khz) < 1:
            log.append(f"skip simplex {r.output_mhz:.4f} {r.call} (WWARA)")
            continue
        ll = getattr(r, "_latlon", None)
        if lat is not None and lon is not None:
            if ll is None:
                log.append(f"skip {r.output_mhz:.4f} {r.call} (WWARA): no coordinates")
                continue
            r.distance_km = haversine_km(lat, lon, *ll)
            if r.distance_km > radius_km:
                continue
        chosen.append(r)
    area_re = re.compile(orrc_areas, re.IGNORECASE) if orrc_areas else None
    for r, rec in orrc:
        if not lo <= r.output_mhz <= hi:
            continue
        if abs(r.offset_khz) < 1:
            log.append(f"skip simplex {r.output_mhz:.4f} {r.call} (ORRC)")
            continue
        if orrc_regions and rec.get("Region", "").strip() not in orrc_regions:
            continue
        area = rec.get("ARRL_Region", "")
        if area_re and area and area.lower() not in ("select one", "null") and not area_re.search(area):
            log.append(f"skip {r.output_mhz:.4f} {r.call} {r.location} (ORRC): area {area!r}")
            continue
        chosen.append(r)
    return merge(chosen, log), log


def merge(reps: list[Repeater], log: list[str]) -> list[Repeater]:
    """One entry per output frequency. Same-frequency entries (a linked
    system with several sites, or the same machine in both lists, or reuse in
    different areas) are combined into one slot, keeping every call/location."""
    by_freq: dict[int, list[Repeater]] = {}
    for r in reps:
        by_freq.setdefault(round(r.output_mhz * 1e4), []).append(r)
    merged = []
    for key in sorted(by_freq):
        group = by_freq[key]
        # Prefer WWARA records (coordinates, mode flags), nearest first.
        group.sort(key=lambda r: (r.source != "WWARA", r.distance_km if r.distance_km is not None else 1e9))
        first = group[0]
        if len(group) > 1:
            seen = []
            for r in group:
                label = f"{r.call} {r.location}".strip()
                if label not in seen:
                    seen.append(label)
            offsets = {r.offset_khz for r in group}
            tones = {r.tone for r in group if r.tone}
            modes = []
            for r in group:
                for m in r.modes:
                    if m not in modes:
                        modes.append(m)
            first = Repeater(
                output_mhz=first.output_mhz,
                input_mhz=first.input_mhz,
                call=" / ".join(dict.fromkeys(r.call for r in group if r.call)),
                location=" / ".join(dict.fromkeys(r.location for r in group if r.location)),
                source="+".join(dict.fromkeys(r.source for r in group)),
                tone=next(iter(tones)) if len(tones) == 1 else None,
                dcs=first.dcs,
                modes=modes,
                distance_km=first.distance_km,
                notes=[n for r in group for n in r.notes],
            )
            if len(seen) > 1:
                first.notes.append("shared output: " + "; ".join(seen))
            if len(offsets) > 1:
                first.notes.append(f"offsets differ {sorted(offsets)} kHz; using {first.offset_khz}")
                log.append(f"{first.output_mhz:.4f}: offsets differ {sorted(offsets)}")
            if len(tones) > 1:
                first.notes.append(f"tones differ {sorted(tones)}")
        merged.append(first)
    return merged


# ----------------------------------------------------------------------------- output


def _q(s: str) -> str:
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


# Digital modes the scanner can decode; other digital-only repeaters are
# listed but not recorded.
DECODED_MODES = {"dmr", "ysf"}


def to_toml(reps: list[Repeater], profile: str, digital_record: bool = False) -> str:
    out = []
    for r in reps:
        name = f"{r.call} {r.location}".strip() if r.call else r.location or f"{r.output_mhz:.4f}"
        if r.distance_km is not None:
            out.append(f"# {r.distance_km:.0f} km")
        for n in r.notes:
            out.append(f"# {n}")
        out.append(f"[[profiles.{profile}.repeaters]]")
        out.append(f"name     = {_q(name)}")
        if r.call:
            out.append(f"call     = {_q(r.call)}")
        if r.location:
            out.append(f"location = {_q(r.location)}")
        out.append(f"source   = {_q(r.source)}")
        out.append(f'output   = "{r.output_mhz:.4f} MHz"')
        out.append(f'offset   = "{r.offset_khz:g} kHz"')
        if r.tone:
            out.append(f"tone     = {r.tone:g}")
        if r.modes:
            out.append("modes    = [" + ", ".join(_q(m) for m in r.modes) + "]")
        mode = r.primary_mode
        if mode != "nbfm":
            out.append(f'mode     = "{mode}"')
            out.append('bandwidth = "11 kHz"')
            if not digital_record and mode not in DECODED_MODES:
                out.append("record   = false  # digital-only until its decoder exists")
        elif r.narrow:
            out.append('bandwidth = "11 kHz"')
            out.append('deviation = "2.5 kHz"')
        out.append("")
    return "\n".join(out)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--wwara", help="WWARA repeater list CSV")
    ap.add_argument("--orrc", help="ORRC Coordinations.xls (HTML table)")
    ap.add_argument("--band", default="2m", choices=sorted(BANDS))
    ap.add_argument("--profile", default="2m", help="profile name to write entries under")
    ap.add_argument("--lat", type=float, help="site latitude (filters WWARA by distance)")
    ap.add_argument("--lon", type=float, help="site longitude")
    ap.add_argument("--radius-km", type=float, default=100.0)
    ap.add_argument("--orrc-regions", default="2,3", help="ORRC Region numbers to keep (comma list, empty = all)")
    ap.add_argument("--orrc-areas", default=DEFAULT_ORRC_AREAS, help="regex on ORRC ARRL_Region ('' = all)")
    ap.add_argument("--record-digital", action="store_true", help="record digital-only repeaters too")
    ap.add_argument("-o", "--out", help="write TOML here instead of stdout")
    ap.add_argument("-v", "--verbose", action="store_true", help="print skipped entries to stderr")
    args = ap.parse_args(argv)
    if not args.wwara and not args.orrc:
        ap.error("give --wwara and/or --orrc")

    def read(path: str) -> str:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read()

    wwara = parse_wwara(read(args.wwara)) if args.wwara else []
    orrc = parse_orrc(read(args.orrc)) if args.orrc else []
    regions = {x.strip() for x in args.orrc_regions.split(",") if x.strip()}
    reps, log = select(wwara, orrc, BANDS[args.band], args.lat, args.lon, args.radius_km, regions, args.orrc_areas)

    header = [
        f"# Generated by tools/channels/import_repeaters.py: {len(reps)} repeaters, band {args.band}",
        f"# Sources: {', '.join(x for x in (args.wwara and 'WWARA', args.orrc and 'ORRC') if x)}",
    ]
    if args.lat is not None:
        header.append(f"# WWARA within {args.radius_km:g} km of {args.lat}, {args.lon}")
    if args.orrc:
        header.append(f"# ORRC regions {sorted(regions) or 'all'}, areas /{args.orrc_areas}/")
    text = "\n".join(header) + "\n\n" + to_toml(reps, args.profile, args.record_digital)
    if args.out:
        with open(args.out, "w") as f:
            f.write(text)
    else:
        sys.stdout.write(text)
    print(
        f"{len(reps)} repeaters ({sum(r.primary_mode != 'nbfm' for r in reps)} digital-only), "
        f"from {len(wwara)} WWARA and {len(orrc)} ORRC records",
        file=sys.stderr,
    )
    if args.verbose:
        for line in log:
            print("  " + line, file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
