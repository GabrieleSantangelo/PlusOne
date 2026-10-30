#!/bin/sh
# Paths taken down and back while a session carries a desktop's load, the
# same each time: two containers, the PC and the tablet, on three Docker
# networks of their own, each shaped with tc both ways: lan (40 Mbit/s,
# 4 ms, 60 ms of queue: a home Wi-Fi), p2p (100 Mbit/s, 1 ms, 30 ms: Wi-Fi
# Direct) and bt (1 Mbit/s, 15 ms, 50 ms), the last carried by socat
# between a UNIX socket on each side: a stream path with no IP at its ends,
# as an RFCOMM socket handed over is. link/core's examples/switchbench on
# each side: the PC's sends a desktop's video (8 Mbit/s, or what the path's
# budget allows), its sound and control messages, and makes the paths as
# plus-one-link does; the tablet's sends the pen's input, and each records
# what comes. Each cycle a path is taken down, held down, brought back;
# tools/switchbench-summary.py sums the cycles up: how soon the path was
# suspect, failed, left; the video's gap, frames lost and broken,
# keyframes; the sound's gaps; input and control messages lost; the
# threads and descriptors of each side at the start and at the end. No
# loss on the networks but the cycles': all that is lost is theirs. Docker
# alone: the host's own network is not touched. Build the examples first
# (tools/link-docker.sh build --release --examples).
#   tools/switchbench.sh SCENARIO [CYCLES] > summary.md
# SCENARIO (CYCLES: 20):
#   wifi  the path the session is on taken down, the other Wi-Fi up: lan,
#         then p2p, in turn (plus-one-link reaches lan first, then p2p);
#   bt    lan taken down, only the Bluetooth-like stream to fall back on; from
#         bt, lan tried each 2 s, the session back on it before each cycle;
#   flap  lan down 2 s and up 2 s, ten times in a row, then ten again; p2p up.
# SWITCHBENCH_HOW: drop (every packet lost both ways: a network out of
# reach, silence; the default), link (the PC's interface down: what a
# Wi-Fi lost tells the system), kill (dropped, and each side's TCP
# connections on it destroyed: both ends see their stream end at once, as
# a cable pulled or a Bluetooth link let go).
# SWITCHBENCH_HOLD: how long a path stays down (s; 10, flap 2);
# SWITCHBENCH_VIDEO: the video's kbit/s on a path with no budget (8000);
# SWITCHBENCH_QUIC=1: the PC moves its TCP paths to QUIC (plus-one-link's
# PLUSONE_LINK_QUIC=1); SWITCHBENCH_OUT: a directory the logs are kept in
# (each side's events and what it sent or received, the cycles' times,
# cycles.jsonl); SWITCHBENCH_BIN: the examples' directory;
# SWITCHBENCH_NAME (sb): the containers' and networks' names;
# SWITCHBENCH_NET (198.19.24): the networks are its .1, .2, .3 /24s.
set -e
cd "$(dirname "$0")/.."
scenario=${1:?a scenario: wifi, bt or flap}
cycles=${2:-20}
name=${SWITCHBENCH_NAME:-sb}
how=${SWITCHBENCH_HOW:-drop}
video=${SWITCHBENCH_VIDEO:-8000}
net=${SWITCHBENCH_NET:-198.19.24}
bin=${SWITCHBENCH_BIN:-$PWD/link/target/release/examples}
case $scenario in
    wifi) paths="lan p2p" upgrade="" hold=${SWITCHBENCH_HOLD:-10} ;;
    bt) paths="lan bt" upgrade="--upgrade-from bt" hold=${SWITCHBENCH_HOLD:-10} ;;
    flap) paths="lan p2p" upgrade="" hold=${SWITCHBENCH_HOLD:-2} ;;
    *) echo "a scenario: wifi, bt or flap" >&2; exit 2 ;;
esac
case $how in
    drop|link|kill) ;;
    *) echo "SWITCHBENCH_HOW: drop, link or kill" >&2; exit 2 ;;
esac
[ -x "$bin/switchbench" ] || { echo "no $bin/switchbench: tools/link-docker.sh build --release --examples" >&2; exit 1; }
quic=""
[ "${SWITCHBENCH_QUIC:-}" != 1 ] || quic=--quic
docker build -q -t plus-one-switchbench - < Dockerfile.switchbench > /dev/null
work=$(mktemp -d)
out=${SWITCHBENCH_OUT:-$work/out}
mkdir -p "$out/pc" "$out/tablet" "$work/w"
out=$(cd "$out" && pwd)
cleanup() {
    docker rm -f "$name-pc" "$name-tablet" > /dev/null 2>&1 || true
    for network in lan p2p bt; do
        docker network rm "$name-$network" > /dev/null 2>&1 || true
    done
    rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
for side in pc tablet; do
    mkdir "$work/w/$side"
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout "$work/w/$side/key.pem" \
        -out "$work/w/$side/cert.pem" -days 2 -subj /CN=bench 2> /dev/null
    openssl x509 -in "$work/w/$side/cert.pem" -outform der | sha256sum | cut -d' ' -f1 > "$work/w/$side/fp"
done
# Inside a container, what is done to a network (found by its prefix):
#   net.sh shape|drop|kill|restore|down|up PREFIX DELAY_MS RATE QUEUE_MS BURST [PEER]
cat > "$work/w/net.sh" << 'END'
set -e
action=$1 prefix=$2 delay=$3 rate=$4 queue=$5 burst=$6 peer=$7
dev=$(ip -o -4 addr show | awk -v p="$prefix." 'index($4, p) == 1 { print $2 }')
case $action in
    shape) tc qdisc add dev "$dev" root handle 1: netem delay "${delay}ms" limit 100000
           tc qdisc add dev "$dev" parent 1:1 handle 10: tbf rate "$rate" burst "$burst" latency "${queue}ms" ;;
    drop) tc qdisc change dev "$dev" root handle 1: netem delay "${delay}ms" loss 100% limit 100000 ;;
    kill) tc qdisc change dev "$dev" root handle 1: netem delay "${delay}ms" loss 100% limit 100000
          ss -K -t dst "$peer" > /dev/null ;;
    restore) tc qdisc change dev "$dev" root handle 1: netem delay "${delay}ms" limit 100000 ;;
    down) ip link set dev "$dev" down ;;
    up) ip link set dev "$dev" up ;;
esac
END
chmod -R a+rX "$work/w"
# A network's /24, and how it is shaped each way: delay, rate, queue, burst.
prefix() {
    case $1 in lan) echo "${net}1" ;; p2p) echo "${net}2" ;; bt) echo "${net}3" ;; esac
}
shape() {
    case $1 in
        lan) echo "$(prefix lan) 4 40mbit 60 64kb" ;;
        p2p) echo "$(prefix p2p) 1 100mbit 30 64kb" ;;
        bt) echo "$(prefix bt) 15 1mbit 50 4kb" ;;
    esac
}
for network in lan p2p bt; do
    docker network create --internal --subnet "$(prefix $network).0/24" "$name-$network" > /dev/null
done
for side in pc tablet; do
    host=10
    [ "$side" = pc ] || host=20
    docker run -d --init --name "$name-$side" --network "$name-lan" --ip "$(prefix lan).$host" --cap-add NET_ADMIN \
        -v "$bin:/b:ro" -v "$work/w:/w:ro" -v "$out/$side:/out" plus-one-switchbench sleep infinity > /dev/null
    for network in p2p bt; do
        docker network connect --ip "$(prefix $network).$host" "$name-$network" "$name-$side"
    done
    for network in lan p2p bt; do
        docker exec "$name-$side" sh /w/net.sh shape $(shape $network)
    done
    docker exec "$name-$side" sh -c 'mkdir -p /run/sb && chmod 777 /run/sb'
done
tablet_bt=$(prefix bt).20
# The bt path's relay: the tablet's UNIX socket (its examples/switchbench
# listens there) reached from the PC's, through TCP on the bt network,
# each socket's room small (the stream's, RFCOMM's, is a few tens of KB).
docker exec -d "$name-tablet" socat -b 4096 \
    "TCP-LISTEN:4710,bind=$tablet_bt,reuseaddr,fork,nodelay,sndbuf=8192" UNIX-CONNECT:/run/sb/bt.sock
docker exec -d "$name-pc" socat -b 4096 \
    UNIX-LISTEN:/run/sb/bt.sock,fork,mode=777,unlink-early "TCP:$tablet_bt:4710,nodelay,sndbuf=8192"
user="$(id -u):$(id -g)"
docker exec -d -u "$user" "$name-tablet" sh -c "exec /b/switchbench tablet --cert /w/tablet/cert.pem \
    --key /w/tablet/key.pem --trust $(cat "$work/w/pc/fp") --out /out --listen 0.0.0.0:47101 \
    --bt /run/sb/bt.sock > /out/stdout.txt 2> /out/link.log"
waited=0
until grep -q listening "$out/tablet/stdout.txt" 2> /dev/null; do
    sleep 0.2
    waited=$((waited + 1))
    [ "$waited" -lt 50 ] || { echo "the tablet's side never listened" >&2; cat "$out/tablet/link.log" >&2; exit 1; }
done
targets=""
for path in $paths; do
    case $path in
        bt) targets="$targets --path bt=unix:/run/sb/bt.sock" ;;
        *) targets="$targets --path $path=$(prefix "$path").20:47101" ;;
    esac
done
docker exec -d -u "$user" "$name-pc" sh -c "exec /b/switchbench pc --cert /w/pc/cert.pem --key /w/pc/key.pem \
    --trust $(cat "$work/w/tablet/fp") --out /out $targets $upgrade --video $video $quic \
    > /out/stdout.txt 2> /out/link.log"
now() {
    docker exec "$name-pc" /b/switchbench now
}
current() {
    cat "$out/pc/current" 2> /dev/null || echo none
}
# Until the session has been on one of the paths given for 3 s (90 s at
# most: then said in the actions, and the cycle goes on all the same).
settle() {
    stable=0 waited=0
    while [ "$stable" -lt 6 ]; do
        case " $1 " in
            *" $(current) "*) stable=$((stable + 1)) ;;
            *) stable=0 ;;
        esac
        sleep 0.5
        waited=$((waited + 1))
        if [ "$waited" -ge 180 ]; then
            echo "unsettled $(current) $(now)" >> "$out/actions.txt"
            return 0
        fi
    done
}
# A path down or back as SWITCHBENCH_HOW says, on both sides at once
# (drop, kill) or the PC's (link): the time it began, from the PC's side.
act() {
    direction=$1 path=$2
    case $how/$direction in
        drop/down) action=drop ;;
        kill/down) action=kill ;;
        drop/up|kill/up) action=restore ;;
        link/down) action=down ;;
        link/up) action=up ;;
    esac
    case $how in
        drop|kill)
            docker exec "$name-tablet" sh /w/net.sh "$action" $(shape "$path") "$(prefix "$path").10" &
            docker exec "$name-pc" sh -c "/b/switchbench now && sh /w/net.sh $action $(shape "$path") $(prefix "$path").20"
            wait ;;
        link)
            docker exec "$name-pc" sh -c "/b/switchbench now && sh /w/net.sh $action $(shape "$path")" ;;
    esac
}
until grep -q loaded "$out/pc/stdout.txt" 2> /dev/null; do
    sleep 0.5
    waited=$((waited + 1))
    [ "$waited" -lt 200 ] || { echo "the PC's side never loaded" >&2; cat "$out/pc/link.log" >&2; exit 1; }
done
echo "start $(now)" > "$out/actions.txt"
settle "$paths"
sleep 3
for cycle in $(seq "$cycles"); do
    case $scenario in
        wifi) settle "lan p2p"; path=$(current) ;;
        bt) settle lan; path=lan ;;
        flap) [ $((cycle % 10)) != 1 ] || { settle "lan p2p"; sleep 3; }; path=lan ;;
    esac
    echo "$cycle down $path $how $(act down "$path")" >> "$out/actions.txt"
    sleep "$hold"
    echo "$cycle up $path $how $(act up "$path")" >> "$out/actions.txt"
    case $scenario in
        flap) sleep 2 ;;
        *) sleep 4 ;;
    esac
done
settle "$paths"
sleep 5
echo "end $(now)" >> "$out/actions.txt"
sleep 1.5
echo "## $scenario ($how, $cycles cycles, held $hold s$([ -z "$quic" ] || echo ", QUIC"))"
python3 -I tools/switchbench-summary.py "$out"
