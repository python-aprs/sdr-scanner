#!/bin/sh
# Build the sdrplay feature against the stand-in header, run `scanner serve`
# for 13 s against a fake API library that streams a keyed 147.52 MHz carrier
# (with one overload event and one 3-packet counter gap), and check that it
# recorded transmissions. Then run it again with the first stream going
# silent after 6 s and the first reopen refused, and check that it reopened
# the radio and kept recording. Needs a C compiler; no SDRplay SDK or radio.
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

# The radio stalls after 6 s; serve notices after 5 s of silence, the first
# reopen fails, the second (2 s later) works, and recording carries on.
FAKE_STALL_AFTER=6 "$root/target/release/scanner" serve --source sdrplay \
    --config "$root/config/rainier.toml" --out "$work/out2" --http off \
    --status-every 5 2>"$work/log2" &
pid=$!
sleep 28
kill -INT $pid
wait $pid
cat "$work/log2"
grep -q "radio reopened" "$work/log2"
# Keyed 1-3 s of every 4 s: two before the stall, more after the reopen.
n2=$(find "$work/out2" -name '147.5200_*.wav' | wc -l)
echo "recorded $n2 transmissions on 147.52 MHz around a reopen"
[ "$n2" -ge 4 ]

# Another program holds the API lock: serve says what it is waiting on after
# 10 s, and Ctrl-C gets it out without a second Ctrl-C.
FAKE_HANG_LOCK=1 "$root/target/release/scanner" serve --source sdrplay \
    --config "$root/config/rainier.toml" --out "$work/out3" --http off 2>"$work/log3" &
pid=$!
sleep 12
kill -INT $pid
rc=0
wait $pid || rc=$?
cat "$work/log3"
grep -q "still waiting for the SDRplay API" "$work/log3"
grep -q "stopped while waiting" "$work/log3"
[ "$rc" -ne 0 ]
echo "a held radio is reported and Ctrl-C quits"
