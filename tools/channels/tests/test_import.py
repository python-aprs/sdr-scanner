import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import import_repeaters as ir

WWARA = """DATA_SPEC_VERSION=2015.2.2
"FC_RECORD_ID","SOURCE","OUTPUT_FREQ","INPUT_FREQ","STATE","CITY","LOCALE","CALL","SPONSOR","CTCSS_IN","CTCSS_OUT","DCS_CDCSS","DTMF","LINK","FM_WIDE","FM_NARROW","DSTAR_DV","DSTAR_DD","DMR","DMR_COLOR_CODE","FUSION","FUSION_DSQ","P25_PHASE_1","P25_PHASE_2","P25_NAC","NXDN_DIGITAL","NXDN_MIXED","NXDN_RAN","ATV","DATV","RACES","ARES","WX","URL","LATITUDE","LONGITUDE","EXPIRATION_DATE","COMMENT"
"1","WWARA","147.1000","147.7000","WA","Longview","SW","W1AAA","","114.8","114.8","","","","Y","N","N","N","N","","N","","N","N","","N","N","","N","N","N","N","N","","46.15","-122.95","2030-01-01",""
"2","WWARA","146.4500","147.4500","WA","Cathlamet","SW","W1BBB","","","","","","","N","N","N","N","Y","1","N","","N","N","","N","N","","N","N","N","N","N","","46.20","-123.38","2030-01-01",""
"3","WWARA","147.3000","147.9000","WA","Woodland","SW","W1CCC","","114.8","","","","","N","Y","N","N","N","","Y","","N","N","","N","N","","N","N","N","N","N","","45.90","-122.74","2030-01-01",""
"4","WWARA","146.9600","146.3600","WA","Seattle","PS","W1DDD","","103.5","","","","","Y","N","N","N","N","","N","","N","N","","N","N","","N","N","N","N","N","","47.60","-122.33","2030-01-01",""
"5","WWARA","146.5200","146.5200","WA","Simplex","SW","W1EEE","","","","","","","Y","N","N","N","N","","N","","N","N","","N","N","","N","N","N","N","N","","46.10","-122.90","2030-01-01",""
"""

ORRC = """<div><table>
<tr><th scope="col">RecID</th><th scope="col">TxFreq</th><th scope="col">RxFreq</th><th scope="col">NearestCity</th><th scope="col">RepeaterCallSign</th><th scope="col">RepeaterName</th><th scope="col">CoordinationHolder</th><th scope="col">Contact</th><th scope="col">Sponsor</th><th scope="col">Region</th><th scope="col">ARRL_Region</th><th scope="col">ARRL_Code</th><th scope="col">PermitDate</th><th scope="col">LastUpdated</th></tr>
<tr><td>&nbsp;</td><td>146.68</td><td>146.08</td><td>St. Helens</td><td>K7AAA</td><td>&nbsp;</td><td></td><td></td><td></td><td>2</td><td>Lower Columbia</td><td>oet(114.8)</td><td></td><td></td></tr>
<tr><td>&nbsp;</td><td>146.62</td><td>146.02</td><td>Gaston</td><td>K7BBB</td><td>&nbsp;</td><td></td><td></td><td></td><td>2</td><td>NW Oregon &amp; SW Washington</td><td>oe(dstar)</td><td></td><td></td></tr>
<tr><td>&nbsp;</td><td>147.26</td><td>147.86</td><td>LaGrande</td><td>K7CCC</td><td>&nbsp;</td><td></td><td></td><td></td><td>2</td><td>North East Oregon</td><td>ot(100)e</td><td></td><td></td></tr>
<tr><td>&nbsp;</td><td>145.17</td><td>144.57</td><td>Eugene</td><td>K7DDD</td><td>&nbsp;</td><td></td><td></td><td></td><td>4</td><td>South Willamette Valley</td><td>oet(123.0)</td><td></td><td></td></tr>
<tr><td>&nbsp;</td><td>147.10</td><td>147.70</td><td>Vancouver WA</td><td>K7EEE</td><td>&nbsp;</td><td></td><td></td><td></td><td>3</td><td>SW Washington</td><td>olt(D023)</td><td></td><td></td></tr>
</table></div>"""

SITE = {
    "lat": 46.089,
    "lon": -122.936,
    "radius_km": 100.0,
    "orrc_regions": {"2", "3"},
    "orrc_areas": ir.DEFAULT_ORRC_AREAS,
}


def run():
    return ir.select(ir.parse_wwara(WWARA), ir.parse_orrc(ORRC), ir.BANDS["2m"], **SITE)


def by_freq(reps):
    return {round(r.output_mhz, 4): r for r in reps}


def test_orrc_code_parsing():
    assert ir.parse_orrc_code("oet(107.2)") == (107.2, None, [])
    assert ir.parse_orrc_code("ot(100)e") == (100.0, None, [])
    assert ir.parse_orrc_code("olt(D023)") == (None, "023", [])
    assert ir.parse_orrc_code("olt(DCS025)") == (None, "025", [])
    assert ir.parse_orrc_code("oe(dstar)") == (None, None, ["DSTAR"])
    assert ir.parse_orrc_code("") == (None, None, [])


def test_distance_region_and_simplex_filters():
    reps, log = run()
    f = by_freq(reps)
    assert 146.96 not in f  # Seattle, > 100 km
    assert 146.52 not in f  # simplex
    assert 147.26 not in f  # ORRC region 2 but North East Oregon
    assert 145.17 not in f  # ORRC region 4
    assert {147.1, 146.45, 147.3, 146.68, 146.62} <= set(f)
    assert any("North East Oregon" in line for line in log)


def test_modes_and_offsets():
    f = by_freq(run()[0])
    assert f[147.1].offset_khz == 600
    assert f[146.68].offset_khz == -600
    assert f[146.45].primary_mode == "dmr" and f[146.45].offset_khz == 1000
    assert f[146.62].primary_mode == "dstar"
    assert f[147.3].primary_mode == "nbfm" and f[147.3].narrow  # FM narrow + Fusion
    assert "DMR CC1" in f[146.45].notes


def test_same_output_from_both_lists_is_merged():
    f = by_freq(run()[0])
    r = f[147.1]
    assert r.source == "WWARA+ORRC"
    assert r.call == "W1AAA / K7EEE"
    assert "Vancouver, WA" in r.location
    assert any("shared output" in n for n in r.notes)


def test_toml_output_is_loadable_shape():
    reps, _ = run()
    text = ir.to_toml(reps, "site")
    assert text.count("[[profiles.site.repeaters]]") == len(reps)
    assert 'mode     = "dmr"' in text
    # D-STAR has no decoder yet, so it isn't recorded; DMR is.
    assert text.count("record   = false") == sum(r.primary_mode not in ("nbfm", "dmr", "ysf", "dstar") for r in reps)
    assert 'deviation = "2.5 kHz"' in text  # narrow FM
    import tomllib

    doc = tomllib.loads(text)
    entries = doc["profiles"]["site"]["repeaters"]
    assert {e["output"] for e in entries} >= {"147.1000 MHz", "146.4500 MHz"}


def test_shared_output_lists_each_site_with_its_tone():
    f = by_freq(run()[0])
    sites = f[147.1].sites
    assert [(s.call, s.tone, s.dcs) for s in sites] == [("W1AAA", 114.8, None), ("K7EEE", None, "023")]
    assert f[147.1].tone is None
    toml = ir.to_toml([f[147.1]], "p")
    assert '{ call = "W1AAA", location = "Longview, WA", tone = 114.8 },' in toml
    assert 'dcs = "023"' in toml


def test_tone_override_fills_a_missing_tone():
    log = []
    reps = ir.parse_wwara(WWARA)
    ir.apply_tones(reps, ["w1ccc@147.30=100", "W1BBB@146.45=D754", "W9ZZZ@147.3=100"], log)
    f = by_freq(reps)
    assert f[147.3].tone == 100.0
    assert (f[146.45].tone, f[146.45].dcs) == (None, "754")
    assert any("W9ZZZ" in line for line in log)


RB = """{"count": 4, "results": [
 {"Frequency": "146.62000", "Input Freq": "146.02000", "PL": "100.0", "TSQ": "", "Callsign": "K7BBB",
  "Nearest City": "Gaston", "State": "Oregon", "Lat": "45.43", "Long": "-123.13", "Operational Status": "On-air",
  "FM Analog": "Yes", "FM Bandwidth": "25 kHz", "DMR": "No", "D-Star": "Yes", "System Fusion": "No",
  "APCO P-25": "No", "NXDN": "No"},
 {"Frequency": "147.36000", "Input Freq": "147.96000", "PL": "D754", "Callsign": "w1zzz",
  "Nearest City": "Kelso", "State": "Washington", "Lat": "46.15", "Long": "-122.90", "Operational Status": "On-air",
  "FM Analog": "Yes", "FM Bandwidth": "12.5 kHz"},
 {"Frequency": "147.38000", "Input Freq": "147.98000", "PL": "123.0", "Callsign": "W1OFF",
  "Nearest City": "Kelso", "State": "Washington", "Lat": "46.15", "Long": "-122.90", "Operational Status": "Off-air",
  "FM Analog": "Yes"},
 {"Frequency": "147.10000", "Input Freq": "147.70000", "PL": "CSQ", "Callsign": "W1AAA",
  "Nearest City": "Longview", "State": "Washington", "Lat": "46.15", "Long": "-122.95", "Operational Status": "On-air",
  "FM Analog": "Yes"}
]}"""


def test_repeaterbook_parsing():
    rb = {r.call: r for r in ir.parse_repeaterbook(RB)}
    assert "W1OFF" not in rb  # off-air
    assert (rb["K7BBB"].tone, rb["K7BBB"].location, rb["K7BBB"].modes) == (100.0, "Gaston, OR", ["FM", "DSTAR"])
    assert (rb["W1ZZZ"].tone, rb["W1ZZZ"].dcs, rb["W1ZZZ"].narrow) == (None, "754", True)
    assert rb["W1AAA"].tone is None  # CSQ


def test_repeaterbook_fills_missing_tones_only():
    reps, _ = ir.select(
        ir.parse_wwara(WWARA), ir.parse_orrc(ORRC), ir.BANDS["2m"], **SITE, repeaterbook=ir.parse_repeaterbook(RB)
    )
    f = by_freq(reps)
    assert f[146.62].tone == 100.0  # ORRC lists none
    assert any("from RepeaterBook" in n for n in f[146.62].notes)
    assert 147.36 not in f  # RepeaterBook-only, not added by default
    # A listed tone is never replaced (W1AAA keeps 114.8 although RepeaterBook says CSQ).
    assert f[147.1].sites[0].tone == 114.8


def test_repeaterbook_all_adds_uncoordinated():
    reps, _ = ir.select(
        ir.parse_wwara(WWARA),
        [],
        ir.BANDS["2m"],
        **SITE,
        repeaterbook=ir.parse_repeaterbook(RB),
        repeaterbook_all=True,
    )
    f = by_freq(reps)
    assert f[147.36].dcs == "754" and f[147.36].source == "RepeaterBook"
    assert 147.38 not in f
    assert f[147.1].source == "WWARA"  # same call and output: not added twice


def test_repeaterbook_alone_is_the_whole_list(tmp_path, capsys):
    rb = tmp_path / "rb.json"
    rb.write_text(RB)
    out = tmp_path / "out.toml"
    assert ir.main(["--repeaterbook", str(rb), "--lat", "46.089", "--lon", "-122.936", "-o", str(out)]) == 0
    text = out.read_text()
    assert 'name     = "W1ZZZ Kelso, WA"' in text and 'name     = "K7BBB Gaston, OR"' in text
    assert "not coordinated" not in text
    assert "W1OFF" not in text
    assert "3 RepeaterBook records" in capsys.readouterr().err


# The website's JSON export (proximity search, Export > JSON).
RB_WEB = """{"schema_version": 1, "export_format": "json", "count": 3, "records": [
 {"callsign": "N7DEM", "freq_mhz": "147.260000", "input_freq": "147.860000", "offset_mhz": "0.60000000000002",
  "tone": "114.8", "city": "Longview", "county": "Cowlitz", "state": "WA", "country": "US", "landmark": "",
  "lat": "46.16120148", "lon": "-122.88700104", "mixed_mode": "Yes", "dmr": "", "dmr_cc": "0", "dstar_node": "",
  "nxdn": "", "p25": "", "ysf": "", "m17": "", "wide_area": "Yes", "pl_tone": "114.8", "tsq_tone": "114.8"},
 {"callsign": "W1DIG", "freq_mhz": "145.330000", "input_freq": "", "offset_mhz": "-0.59999999999999",
  "tone": "CSQ", "city": "Kelso", "state": "WA", "lat": "46.15", "lon": "-122.90", "mixed_mode": "",
  "dmr": "Yes", "dmr_cc": "1", "pl_tone": "CSQ"},
 {"callsign": "W1ANA", "freq_mhz": "146.760000", "input_freq": "146.160000", "tone": "D023",
  "city": "Rainier", "state": "OR", "lat": "46.09", "lon": "-122.94", "mixed_mode": "", "pl_tone": "D023"}
]}"""


def test_repeaterbook_website_export():
    rb = {r.call: r for r in ir.parse_repeaterbook(RB_WEB)}
    n = rb["N7DEM"]
    assert (n.output_mhz, n.input_mhz, n.tone, n.location, n.modes) == (147.26, 147.86, 114.8, "Longview, WA", ["FM"])
    d = rb["W1DIG"]
    assert d.modes == ["DMR"] and d.primary_mode == "dmr" and d.tone is None
    assert d.offset_khz == -600  # input from the offset
    a = rb["W1ANA"]
    assert a.modes == ["FM"] and a.dcs == "023"
    assert a._latlon == (46.09, -122.94)


def test_output_tone_where_it_differs():
    # 147.18: NM7R takes 82.5 but transmits 103.5; W7LT uses 103.5 both ways.
    web = """{"records": [
     {"callsign": "NM7R", "freq_mhz": "147.18", "input_freq": "147.78", "city": "Chinook", "state": "WA",
      "lat": "46.29", "lon": "-123.90", "mixed_mode": "Yes", "pl_tone": "82.5", "tsq_tone": "103.5"},
     {"callsign": "W7LT", "freq_mhz": "147.18", "input_freq": "147.78", "city": "Clackamas", "state": "OR",
      "lat": "45.45", "lon": "-122.55", "mixed_mode": "Yes", "pl_tone": "103.5", "tsq_tone": "103.5"}]}"""
    reps, _ = ir.select([], [], ir.BANDS["2m"], **SITE, repeaterbook=ir.parse_repeaterbook(web), repeaterbook_all=True)
    toml = ir.to_toml(reps, "p")
    assert '{ call = "NM7R", location = "Chinook, WA", tone = 82.5, tone_out = 103.5 },' in toml
    assert '{ call = "W7LT", location = "Clackamas, OR", tone = 103.5 },' in toml


def test_tone_override_with_output_tone():
    reps = ir.parse_wwara(WWARA)
    ir.apply_tones(reps, ["W1AAA@147.10=82.5/103.5", "W1CCC@147.3=D023/D754"], [])
    f = by_freq(reps)
    assert (f[147.1].tone, f[147.1].tone_out) == (82.5, 103.5)
    assert (f[147.3].dcs, f[147.3].dcs_out, f[147.3].tone) == ("023", "754", None)
    try:
        ir.apply_tones(reps, ["W1AAA@147.10=82.5/xyz"], [])
    except ValueError as e:
        assert "CALL@MHZ" in str(e)
    else:
        raise AssertionError("bad output tone accepted")


def test_exclude_drops_one_site_of_a_shared_output():
    reps, log = ir.select(ir.parse_wwara(WWARA), ir.parse_orrc(ORRC), ir.BANDS["2m"], **SITE, excludes=["k7eee@147.10"])
    r = by_freq(reps)[147.1]
    assert (r.call, r.source, r.sites) == ("W1AAA", "WWARA", [])
    _, log = ir.select(ir.parse_wwara(WWARA), [], ir.BANDS["2m"], **SITE, excludes=["W9ZZZ@147.10"])
    assert any("W9ZZZ" in line for line in log)
