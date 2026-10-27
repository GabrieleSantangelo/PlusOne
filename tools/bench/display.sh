#!/bin/bash
# The PC's side of the desktop, Python against Rust, against the same fake
# tablet on this PC (plus-one-rs fake-app, docs/RUST.md):
#   tools/bench/display.sh python|rust SIZE BITRATE SECONDS [ENCODER]
# e.g. tools/bench/display.sh rust 1920x1200 20000 20
set -u
impl=$1 size=$2 bitrate=$3 seconds=$4 encoder=${5:-auto}
repo=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
export XDG_CONFIG_HOME=$work/config  # an identity and pairing of its own: not the user's
port=$((48000 + RANDOM % 1000))
$repo/rust/target/release/plus-one-rs fake-app $work/app --port $port --screen 3840x2400 > $work/app.out 2> $work/app.err &
app=$!
for i in $(seq 50); do [ -f $work/app/fingerprint ] && break; sleep 0.1; done
mkdir -p $XDG_CONFIG_HOME/plus-one/companion
printf '{"%s": {"name": "Bench", "app_at": "127.0.0.1:%s"}}' "$(cat $work/app/fingerprint)" $port \
    > $XDG_CONFIG_HOME/plus-one/companion/paired.json
if [ "$impl" = python ]; then
    cmd="python3 -m plusone.appdisplay 127.0.0.1 --port $port --test --seconds $seconds --bitrate $bitrate --size $size --encoder $encoder"
else
    cmd="$repo/rust/target/release/plus-one-rs display --at 127.0.0.1:$port --test --seconds $seconds --bitrate $bitrate --size $size --encoder $encoder"
fi
cd $repo
TIMEFORMAT="cpu user %U s, sys %S s, wall %R s"
{ time $cmd > $work/pc.out 2>&1; } 2> $work/time
wait $app
printf '%-6s %-9s %6s kbit/s  %s  %s\n' "$impl" "$size" "$bitrate" "$(cat $work/time)" "$(cat $work/app.out)"
grep -h -E "error|Error|panicked" $work/pc.out $work/app.err | head -3
rm -rf "${work:?}"
