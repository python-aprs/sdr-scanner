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
import json
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
class Site:
    """One of the repeaters sharing an output frequency."""

    call: str
    location: str
    tone: float | None = None
    dcs: str | None = None

    @property
    def label(self) -> str:
        return f"{self.call} {self.location}".strip()


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
    # For an output shared by several repeaters: one per call.
    sites: list[Site] = field(default_factory=list)

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


# ----------------------------------------------------------------------------- RepeaterBook


def _rb_tone(value: str) -> tuple[float | None, str | None]:
    """RepeaterBook's `PL`/`TSQ`: a CTCSS frequency, `D023` for DCS, or
    empty / `CSQ` for none."""
    v = (value or "").strip().upper()
    m = re.fullmatch(r"D(\d{3})[NI]?", v)
    if m:
        return None, m.group(1)
    f = _float(v)
    return (f, None) if f and 60.0 <= f <= 260.0 else (None, None)


def parse_repeaterbook(text: str) -> list[Repeater]:
    """Records from RepeaterBook's export API (`{"results": [...]}`, or a
    bare list). Off-air records are skipped."""
    data = json.loads(text)
    records = data.get("results", []) if isinstance(data, dict) else data
    out = []
    for rec in records or []:
        if not isinstance(rec, dict):
            continue
        status = str(rec.get("Operational Status", "")).strip().lower()
        if status and status != "on-air":
            continue
        f_out = _float(str(rec.get("Frequency", "")))
        f_in = _float(str(rec.get("Input Freq", ""))) or f_out
        if f_out is None:
            continue
        tone, dcs = _rb_tone(str(rec.get("PL", "")))

        def yes(k: str, rec: dict = rec) -> bool:
            return str(rec.get(k, "")).strip().lower() == "yes"

        modes = []
        if yes("FM Analog"):
            modes.append("NFM" if "12.5" in str(rec.get("FM Bandwidth", "")) else "FM")
        if yes("DMR"):
            modes.append("DMR")
        if yes("System Fusion"):
            modes.append("YSF")
        if yes("D-Star"):
            modes.append("DSTAR")
        if yes("APCO P-25"):
            modes.append("P25")
        if yes("NXDN"):
            modes.append("NXDN")
        city = str(rec.get("Nearest City", "")).strip()
        state = STATE_ABBR.get(str(rec.get("State", "")).strip(), str(rec.get("State", "")).strip())
        r = Repeater(
            output_mhz=f_out,
            input_mhz=f_in,
            call=str(rec.get("Callsign", "")).strip().upper(),
            location=f"{city}, {state}".strip(", "),
            source="RepeaterBook",
            tone=tone,
            dcs=dcs,
            modes=modes,
        )
        lat, lon = _float(str(rec.get("Lat", ""))), _float(str(rec.get("Long", "")))
        r._latlon = (lat, lon) if lat is not None and lon is not None else None  # type: ignore[attr-defined]
        out.append(r)
    return out


STATE_ABBR = {"Washington": "WA", "Oregon": "OR", "Idaho": "ID", "California": "CA"}


def fill_tones(chosen: list[Repeater], rb: list[Repeater], log: list[str]) -> None:
    """Give coordinated repeaters that list no tone the tone RepeaterBook
    has for the same call on the same output."""
    by_key: dict[tuple[str, int], Repeater] = {}
    for r in rb:
        if r.tone or r.dcs:
            by_key.setdefault((r.call.upper(), round(r.output_mhz * 1e4)), r)
    for r in chosen:
        if r.tone or r.dcs:
            continue
        hit = by_key.get((r.call.upper(), round(r.output_mhz * 1e4)))
        if hit:
            r.tone, r.dcs = hit.tone, hit.dcs
            label = f"{hit.tone:g}" if hit.tone else f"D{hit.dcs}"
            r.notes.append(f"{r.call} tone {label} from RepeaterBook")
            log.append(f"{r.output_mhz:.4f} {r.call}: tone {label} from RepeaterBook")


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
    tones: list[str] | None = None,
    repeaterbook: list[Repeater] | None = None,
    repeaterbook_all: bool = False,
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
    rb = repeaterbook or []
    fill_tones(chosen, rb, log)
    if repeaterbook_all:
        known = {(r.call.upper(), round(r.output_mhz * 1e4)) for r in chosen}
        for r in rb:
            if not lo <= r.output_mhz <= hi or abs(r.offset_khz) < 1:
                continue
            if (r.call.upper(), round(r.output_mhz * 1e4)) in known:
                continue
            ll = getattr(r, "_latlon", None)
            if lat is not None and lon is not None:
                if ll is None:
                    continue
                r.distance_km = haversine_km(lat, lon, *ll)
                if r.distance_km > radius_km:
                    continue
            r.notes.append(f"{r.call} only in RepeaterBook (not coordinated)")
            known.add((r.call.upper(), round(r.output_mhz * 1e4)))
            chosen.append(r)
    apply_tones(chosen, tones or [], log)
    return merge(chosen, log), log


def merge(reps: list[Repeater], log: list[str]) -> list[Repeater]:
    """One entry per output frequency. Same-frequency entries (a linked
    system with several sites, or the same machine in both lists, or reuse in
    different areas) are combined into one slot, keeping every call/location.
    Each distinct call becomes one of the entry's `sites` with its own tone,
    so the scanner can tell from the tone it hears which repeater it was."""
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
            # One site per call: the same machine in both lists, or one
            # call's linked sites, is one site (its locations joined), taking
            # the tone from whichever record has it.
            sites: dict[str, Site] = {}
            for r in group:
                k = r.call or r.location
                s = sites.get(k)
                if s is None:
                    sites[k] = Site(r.call, r.location, r.tone, r.dcs)
                else:
                    if r.location and r.location not in s.location.split(" / "):
                        s.location = f"{s.location} / {r.location}" if s.location else r.location
                    s.tone = s.tone or r.tone
                    s.dcs = s.dcs or r.dcs
            offsets = {r.offset_khz for r in group}
            modes = []
            for r in group:
                for m in r.modes:
                    if m not in modes:
                        modes.append(m)
            site_list = list(sites.values())
            first = Repeater(
                output_mhz=first.output_mhz,
                input_mhz=first.input_mhz,
                call=" / ".join(dict.fromkeys(r.call for r in group if r.call)),
                location=" / ".join(dict.fromkeys(r.location for r in group if r.location)),
                source="+".join(dict.fromkeys(r.source for r in group)),
                tone=site_list[0].tone if len(site_list) == 1 else None,
                dcs=site_list[0].dcs if len(site_list) == 1 else None,
                modes=modes,
                distance_km=first.distance_km,
                notes=[n for r in group for n in r.notes],
                sites=site_list if len(site_list) > 1 else [],
            )
            if len(site_list) > 1:
                first.notes.append("shared output: " + "; ".join(s.label for s in site_list))
                if not all(s.tone or s.dcs for s in site_list):
                    first.notes.append("a site has no tone listed: add it with --tone so recordings can be told apart")
            if len(offsets) > 1:
                first.notes.append(f"offsets differ {sorted(offsets)} kHz; using {first.offset_khz}")
                log.append(f"{first.output_mhz:.4f}: offsets differ {sorted(offsets)}")
        merged.append(first)
    return merged


def apply_tones(reps: list[Repeater], overrides: list[str], log: list[str]) -> None:
    """Apply `--tone CALL@MHZ=TONE` overrides (TONE is a CTCSS frequency or a
    DCS code like D023) for tones the coordinator lists leave out or get wrong."""
    for o in overrides:
        m = re.fullmatch(r"\s*([^@=\s]+)\s*@\s*([\d.]+)\s*=\s*(D?)(\d+(?:\.\d+)?)\s*", o, re.IGNORECASE)
        if not m:
            raise ValueError(f"--tone {o!r}: expected CALL@MHZ=TONE, e.g. W7LT@147.18=103.5 or K7EEE@147.10=D023")
        call, mhz, is_dcs, value = m.group(1).upper(), float(m.group(2)), m.group(3), m.group(4)
        hit = False
        for r in reps:
            if r.call.upper() == call and abs(r.output_mhz - mhz) < 0.0005:
                if is_dcs:
                    r.tone, r.dcs = None, value.zfill(3)
                else:
                    r.tone, r.dcs = float(value), None
                r.notes.append(f"{r.call} tone {is_dcs.upper()}{value} from --tone")
                hit = True
        if not hit:
            log.append(f"--tone {o}: no {call} on {mhz:.4f} MHz")


# ----------------------------------------------------------------------------- output


def _q(s: str) -> str:
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


# Digital modes the scanner can decode; other digital-only repeaters are
# listed but not recorded.
DECODED_MODES = {"dmr", "ysf", "dstar"}


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
        elif r.dcs:
            out.append(f'dcs      = "{r.dcs}"')
        if r.sites:
            out.append("sites    = [")
            for s in r.sites:
                fields = [f"call = {_q(s.call)}"] if s.call else []
                if s.location:
                    fields.append(f"location = {_q(s.location)}")
                if s.tone:
                    fields.append(f"tone = {s.tone:g}")
                elif s.dcs:
                    fields.append(f'dcs = "{s.dcs}"')
                out.append("    { " + ", ".join(fields) + " },")
            out.append("]")
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
    ap.add_argument(
        "--repeaterbook",
        action="append",
        default=[],
        metavar="JSON",
        help="RepeaterBook export (fetch_repeaterbook.py) to fill in missing tones from; repeatable",
    )
    ap.add_argument(
        "--repeaterbook-all",
        action="store_true",
        help="also add repeaters only RepeaterBook lists (in range, on-air), not just their tones",
    )
    ap.add_argument("--band", default="2m", choices=sorted(BANDS))
    ap.add_argument("--profile", default="2m", help="profile name to write entries under")
    ap.add_argument("--lat", type=float, help="site latitude (filters WWARA by distance)")
    ap.add_argument("--lon", type=float, help="site longitude")
    ap.add_argument("--radius-km", type=float, default=100.0)
    ap.add_argument("--orrc-regions", default="2,3", help="ORRC Region numbers to keep (comma list, empty = all)")
    ap.add_argument("--orrc-areas", default=DEFAULT_ORRC_AREAS, help="regex on ORRC ARRL_Region ('' = all)")
    ap.add_argument("--record-digital", action="store_true", help="record digital-only repeaters too")
    ap.add_argument(
        "--tone",
        action="append",
        default=[],
        metavar="CALL@MHZ=TONE",
        help="set a repeater's tone where the lists lack it, e.g. W7LT@147.18=103.5 or K7EEE@147.10=D023 (repeatable)",
    )
    ap.add_argument("-o", "--out", help="write TOML here instead of stdout")
    ap.add_argument("-v", "--verbose", action="store_true", help="print skipped entries to stderr")
    args = ap.parse_args(argv)
    if not args.wwara and not args.orrc and not (args.repeaterbook and args.repeaterbook_all):
        ap.error("give --wwara and/or --orrc (or --repeaterbook with --repeaterbook-all)")

    def read(path: str) -> str:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read()

    wwara = parse_wwara(read(args.wwara)) if args.wwara else []
    orrc = parse_orrc(read(args.orrc)) if args.orrc else []
    regions = {x.strip() for x in args.orrc_regions.split(",") if x.strip()}
    rb = [r for path in args.repeaterbook for r in parse_repeaterbook(read(path))]
    reps, log = select(
        wwara,
        orrc,
        BANDS[args.band],
        args.lat,
        args.lon,
        args.radius_km,
        regions,
        args.orrc_areas,
        args.tone,
        rb,
        args.repeaterbook_all,
    )

    header = [
        f"# Generated by tools/channels/import_repeaters.py: {len(reps)} repeaters, band {args.band}",
        "# Sources: "
        + ", ".join(
            x
            for x in (
                args.wwara and "WWARA",
                args.orrc and "ORRC",
                args.repeaterbook and ("RepeaterBook" if args.repeaterbook_all else "RepeaterBook (tones)"),
            )
            if x
        ),
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
