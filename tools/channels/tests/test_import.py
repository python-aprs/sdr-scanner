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
    assert "record   = false" in text
    assert 'deviation = "2.5 kHz"' in text  # narrow FM
    import tomllib

    doc = tomllib.loads(text)
    entries = doc["profiles"]["site"]["repeaters"]
    assert {e["output"] for e in entries} >= {"147.1000 MHz", "146.4500 MHz"}
