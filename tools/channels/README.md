# Repeater list importer

`import_repeaters.py` turns coordinator exports into band-profile repeater
entries for `config/*.toml`. Each repeater becomes two slots: its output and
its input (output + offset), so the scanner records the stations keying the
repeater as well as the repeater itself.

Inputs:

- **WWARA** (Washington): the CSV export from wwara.org. Has coordinates and
  per-mode flags, so it is filtered by distance from the site.
- **ORRC** (Oregon): `ORRC_Coordinations.xls` from orrc.org (an HTML table
  despite the extension). No coordinates or mode flags, so it is filtered by
  Region number and the ARRL_Region text, and the tone/mode are parsed from
  the ARRL_Code column.

Regenerate the Rainier entries (2 m and 70 cm):

```sh
python3 tools/channels/import_repeaters.py --wwara WWARA-rptrlist.csv --orrc ORRC_Coordinations.xls \
    --lat 46.089 --lon -122.936 --radius-km 100 --band 2m --profile rainier-2m -v > /tmp/2m.toml
python3 tools/channels/import_repeaters.py --wwara WWARA-rptrlist.csv --orrc ORRC_Coordinations.xls \
    --lat 46.089 --lon -122.936 --radius-km 100 --band 70cm --profile rainier-70cm -v > /tmp/70cm.toml
```

then replace the generated sections at the bottom of `config/rainier.toml` and
run `scanner config check config/rainier.toml`.

Rules:

- Simplex entries (input = output) are skipped; list those as channels.
- Entries sharing an output frequency (a linked multi-site system, the same
  machine in both lists, or frequency reuse on opposite sides of the river)
  are merged into one slot that keeps every call and location. CTCSS tagging
  (Phase 4) will tell them apart in recordings.
- Repeaters with FM are `mode = "nbfm"` (narrow FM gets 11 kHz / 2.5 kHz);
  digital-only ones get their digital mode, and `record = false` unless the
  scanner decodes it (DMR does; `--record-digital` records the rest too).

Tests: `python3 -m pytest tools/channels/tests`.
