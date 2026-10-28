#!/bin/sh
# The link measured on an emulated network, the same each time: two
# containers on a Docker network of their own, each shaping its way out
# with tc as the profile says (netem: delay, loss; tbf: rate, and the
# router's queue), link/core's examples/tablet in one and
# examples/pathbench in the other; TCP, then QUIC; RUNS times. Docker
# alone: the host's own network is not touched. Build the examples first
# (tools/link-docker.sh build --release --examples).
#   tools/netbench.sh PROFILE [RUNS] [SECONDS] [VIDEO_KBPS] [OPENS]
# PROFILE, each way: tim (3 Mbit/s, 10 ms, 0.5 % lost, 300 ms of queue: a
# home Wi-Fi far from its router), hotspot (50 Mbit/s, 5 ms, 0.1 %, 80 ms),
# p2p (300 Mbit/s, 1 ms, 0.05 %, 20 ms), or any as RATE,DELAY_MS,LOSS_%,QUEUE_MS
# (tools/netbench-grid.sh runs a grid of them). VIDEO_KBPS: the video's bitrate
# (else 0.85 of the session's rate on QUIC, 2 Mbit/s on TCP).
set -e
cd "$(dirname "$0")/.."
profile=${1:?a profile: tim, hotspot or p2p}
runs=${2:-3}
seconds=${3:-6}
video=${4:-}
opens=${5:-20}
case $profile in
    tim) rate=3mbit; delay=10; loss=0.5; queue=300 ;;
    hotspot) rate=50mbit; delay=5; loss=0.1; queue=80 ;;
    p2p) rate=300mbit; delay=1; loss=0.05; queue=20 ;;
    *,*,*,*) IFS=, read -r rate delay loss queue extra <<END
$profile
END
        profile=$(echo "$profile" | tr , _) ;;
    *) echo "a profile: tim, hotspot, p2p or RATE,DELAY_MS,LOSS_%,QUEUE_MS[,steps|bursts|cross]" >&2; exit 2 ;;
esac
# extra: steps (the rate a third every 5 s, then back: a Wi-Fi losing its
# signal), bursts (packets let go in slots of 1-4 ms, as Wi-Fi aggregates),
# cross (a TCP download beside, the same way as the video: someone else at
# home).
slot=""
[ "${extra:-}" = bursts ] && slot="slot 1ms 4ms"
low=$(echo "$rate" | awk '{ n = $1 + 0; sub(/[0-9.]+/, "", $1); print int(n / 3 + 0.5) $1 }')
docker build -q -t plus-one-netbench - < Dockerfile.netbench > /dev/null
docker network inspect po-bench > /dev/null 2>&1 || docker network create po-bench > /dev/null
bin=$PWD/link/target/release/examples
work=$(mktemp -d)
trap 'docker rm -f po-tablet > /dev/null 2>&1; rm -rf "$work"' EXIT
for side in pc tablet; do
    mkdir "$work/$side"
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout "$work/$side/key.pem" \
        -out "$work/$side/cert.pem" -days 2 -subj /CN=bench 2> /dev/null
    openssl x509 -in "$work/$side/cert.pem" -outform der | sha256sum | cut -d' ' -f1 > "$work/$side/fp"
done
chmod -R a+rX "$work"
shape="tc qdisc add dev eth0 root handle 1: netem delay ${delay}ms loss ${loss}% $slot limit 100000 \
       && tc qdisc add dev eth0 parent 1:1 handle 10: tbf rate $rate burst 64kb latency ${queue}ms"
if [ "${extra:-}" = steps ]; then
    shape="$shape && { (while sleep 5; do tc qdisc change dev eth0 parent 1:1 handle 10: tbf rate $low burst 64kb \
       latency ${queue}ms; sleep 5; tc qdisc change dev eth0 parent 1:1 handle 10: tbf rate $rate burst 64kb \
       latency ${queue}ms; done) & }"
fi
cross_tablet=""
cross_pc=""
if [ "${extra:-}" = cross ]; then
    cross_tablet="(nc -lk 5001 > /dev/null &) &&"
    cross_pc="{ (sleep 1; cat /dev/zero | nc po-tablet 5001 > /dev/null) & } &&"
fi
for run in $(seq "$runs"); do
    for transport in tcp quic; do
        docker rm -f po-tablet > /dev/null 2>&1 || true
        docker run -d --name po-tablet --network po-bench --cap-add NET_ADMIN -v "$bin:/b:ro" -v "$work:/w:ro" \
            plus-one-netbench sh -c "$shape && $cross_tablet exec /b/tablet --cert /w/tablet/cert.pem --key /w/tablet/key.pem \
            --trust $(cat "$work/pc/fp") --listen 0.0.0.0:47101 --quic" > /dev/null
        sleep 1
        ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' po-tablet)
        flag=""
        [ "$transport" = quic ] && flag=--quic
        echo "== $profile run $run $transport"
        docker run --rm --network po-bench --cap-add NET_ADMIN -v "$bin:/b:ro" -v "$work:/w:ro" plus-one-netbench \
            sh -c "$shape && $cross_pc /b/pathbench --cert /w/pc/cert.pem --key /w/pc/key.pem --trust $(cat "$work/tablet/fp") \
            --tablet $ip:47101 --seconds $seconds --opens $opens $flag ${video:+--video $video}" || echo "test=failed"
    done
done
