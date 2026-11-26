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

- **RepeaterBook** (optional): the JSON its export API returns, saved by
  `fetch_repeaterbook.py`, or the website's JSON export (proximity search, Export, JSON; it carries no status field, so limit the search to on-air repeaters). By default it only fills in tones the
  coordinator lists leave out (same call on the same output);
  `--repeaterbook-all` also adds on-air repeaters only RepeaterBook lists,
  filtered by distance like WWARA. A tone a coordinator lists is never
  replaced, and `--tone` beats both.

Fetch RepeaterBook (cached 12 h, requests 30 s apart; `--token` or `RB_TOKEN`
if the API asks for one):

```sh
python3 tools/channels/fetch_repeaterbook.py --state Washington --state Oregon -o repeaterbook/
```

Regenerate the Rainier entries (2 m and 70 cm), adding
`--repeaterbook repeaterbook/Washington.json --repeaterbook repeaterbook/Oregon.json`
when fetched:

```sh
python3 tools/channels/import_repeaters.py --wwara WWARA-rptrlist.csv --orrc ORRC_Coordinations.xls \
    --lat 46.089 --lon -122.936 --radius-km 100 --band 2m --profile rainier-2m --tone W7LT@147.18=103.5 -v > /tmp/2m.toml
python3 tools/channels/import_repeaters.py --wwara WWARA-rptrlist.csv --orrc ORRC_Coordinations.xls \
    --lat 46.089 --lon -122.936 --radius-km 100 --band 70cm --profile rainier-70cm --tone W7LT@147.18=103.5 -v > /tmp/70cm.toml
```

then replace the generated sections at the bottom of `config/rainier.toml` and
run `scanner config check config/rainier.toml`.

Rules:

- Simplex entries (input = output) are skipped; list those as channels.
- Entries sharing an output frequency (a linked multi-site system, the same
  machine in both lists, or frequency reuse on opposite sides of the river)
  are merged into one slot that keeps every call and location. Each distinct
  call becomes one of the entry's `sites`, with its own tone, and the
  scanner names each recording on that slot after the site whose tone it
  carried (`103.5` on 147.18 is W7LT, `82.5` is NM7R). With no tone, or a
  tone no site lists, the recording keeps the combined name.
- Where the lists leave a tone out (ORRC often does), give it with
  `--tone CALL@MHZ=TONE` (CTCSS Hz, or `D023` for DCS); the Rainier files
  use `--tone W7LT@147.18=103.5`. A shared output with a site that still has
  no tone is flagged in a comment.
- After changing a site's tone, `scanner config retag config/rainier.toml
  <recordings folder>` renames recordings already made (`-n` to preview).
- Repeaters with FM are `mode = "nbfm"` (narrow FM gets 11 kHz / 2.5 kHz);
  digital-only ones get their digital mode, and `record = false` unless the
  scanner decodes it (DMR does; `--record-digital` records the rest too).

Tests: `python3 -m pytest tools/channels/tests`.
