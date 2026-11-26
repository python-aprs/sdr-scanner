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
    --lat 46.089 --lon -122.936 --radius-km 100 --band 2m --profile rainier-2m --exclude NM7R@147.18 --tone W7LT@147.18=103.5 -v > /tmp/2m.toml
python3 tools/channels/import_repeaters.py --wwara WWARA-rptrlist.csv --orrc ORRC_Coordinations.xls \
    --lat 46.089 --lon -122.936 --radius-km 100 --band 70cm --profile rainier-70cm --exclude NM7R@147.18 --tone W7LT@147.18=103.5 -v > /tmp/70cm.toml
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
  carried (on 145.45, `118.8` is W7BU and `110.9` is KB7CNN). With no tone, or a
  tone no site lists, the recording keeps the combined name.
- A site's `tone` is its access tone, which stations send on the input. A
  repeater that transmits a different tone on its output gets `tone_out`
  (from WWARA's CTCSS_OUT or RepeaterBook's TSQ). Output recordings are
  matched on what the repeater transmits, input recordings on the access
  tone. If two sites transmit the same tone, output recordings keep both
  names, and only the input can tell them apart.
- Where the lists leave a tone out or get it wrong, give it with
  `--tone CALL@MHZ=TONE[/OUT]` (CTCSS Hz, or `D023` for DCS, e.g.
  `NM7R@147.18=82.5/103.5`). A shared output with a site that still has no
  tone is flagged in a comment.
- `--exclude CALL@MHZ` drops a repeater from every list, for a wrong listing
  or a site sharing an output that doesn't reach this one. The Rainier files
  use `--exclude NM7R@147.18 --tone W7LT@147.18=103.5`. NM7R Megler Mtn
  doesn't make it here, and it transmits the same 103.5 as W7LT Mt Scott, so
  147.18 is W7LT alone. ORRC lists no tone for W7LT.
- After regenerating, `scanner config retag config/rainier.toml <recordings
  folder>` renames recordings already made to the new names (`-n` to
  preview).
- Repeaters with FM are `mode = "nbfm"` (narrow FM gets 11 kHz / 2.5 kHz);
  digital-only ones get their digital mode, and `record = false` unless the
  scanner decodes it (DMR does; `--record-digital` records the rest too).

Tests: `python3 -m pytest tools/channels/tests`.
