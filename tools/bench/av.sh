#!/bin/bash
# The desktop and the speakers at once, Python against Rust, against the
# same fake tablet on this PC: how late and how unevenly the sound comes
# while the desktop's frames go out (docs/RUST.md).
#   tools/bench/av.sh python|rust SIZE BITRATE SECONDS [camera] [input]
#     camera: a camera's frames written, as the app does (1080p, 30 fps)
#     input: the fake tablet's fingers and pen, 285 packets a second, taken
set -u
impl=$1 size=$2 bitrate=$3 seconds=$4
shift 4
extra="$*"
input_hz=0
case " $extra " in *" input "*) input_hz=285 ;; esac
repo=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
export XDG_CONFIG_HOME=$work/config  # an identity and pairing of its own: not the user's
port=$((48000 + RANDOM % 1000))
$repo/rust/target/release/plus-one-rs fake-app $work/app --port $port --screen 3840x2400 --seconds $((seconds + 6)) --input-hz $input_hz \
    > $work/app.out 2> $work/app.err &
app=$!
for i in $(seq 50); do [ -f $work/app/fingerprint ] && break; sleep 0.1; done
mkdir -p $XDG_CONFIG_HOME/plus-one/companion
printf '{"%s": {"name": "Bench", "app_at": "127.0.0.1:%s"}}' "$(cat $work/app/fingerprint)" $port \
    > $XDG_CONFIG_HOME/plus-one/companion/paired.json
if [ "$impl" = python ]; then
    cmd="env PYTHONPATH=$repo python3 $repo/tools/bench/av.py 127.0.0.1 $port $seconds $bitrate $size $extra"
else
    cmd="$repo/rust/target/release/plus-one-rs bench-av --at 127.0.0.1:$port --seconds $seconds --bitrate $bitrate --size $size"
    for x in $extra; do cmd="$cmd --$x"; done
fi
cd $repo
TIMEFORMAT="cpu user %U s, sys %S s, wall %R s"
{ time $cmd > $work/pc.out 2>&1; } 2> $work/time
wait $app
echo "$impl $size $bitrate kbit/s $extra: $(cat $work/time)"
sed 's/^/  /' $work/app.out
grep -h -E "rror|panicked" $work/pc.out $work/app.err | head -3
rm -rf "${work:?}"
