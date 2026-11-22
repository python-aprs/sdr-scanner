#!/bin/sh
# Build the sdrplay feature against the stand-in header, run `scanner serve`
# for 13 s against a fake API library that streams a keyed 147.52 MHz carrier
# (with one overload event and one 3-packet counter gap), and check that it
# recorded transmissions. Needs a C compiler; no SDRplay SDK or radio.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
case "$(uname)" in Darwin) ext=dylib ;; *) ext=so.3 ;; esac
cc -O2 -shared -fPIC -I"$here" -o "$work/libsdrplay_api.$ext" "$here/fake_api.c" -lm -lpthread
(cd "$root" && SDRPLAY_API_INCLUDE="$here" cargo build -q --release -p scanner-cli --features sdrplay)
export SDRPLAY_API_LIB="$work/libsdrplay_api.$ext"
"$root/target/release/scanner" devices
"$root/target/release/scanner" serve --source sdrplay --config "$root/config/rainier.toml" \
    --out "$work/out" --http off --status-every 5 &
pid=$!
sleep 13
kill -INT $pid
wait $pid
n=$(find "$work/out" -name '147.5200_*.wav' | wc -l)
echo "recorded $n transmissions on 147.52 MHz"
[ "$n" -ge 2 ]
