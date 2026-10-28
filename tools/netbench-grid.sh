#!/bin/sh
# tools/netbench.sh on a grid of networks, each RUNS times, summed up: the
# link's parameters are kept only if they hold on all of them, not on one.
#   tools/netbench-grid.sh [RUNS] [SECONDS] > grid.txt
set -e
cd "$(dirname "$0")/.."
runs=${1:-2}
seconds=${2:-6}
logs=$(mktemp -d)
trap 'rm -rf "$logs"' EXIT
# name rate,delay ms,loss %,queue ms[,steps|bursts|cross]. From about 15 Mbit/s up (a home ADSL
# or Wi-Fi, a phone's hotspot, Wi-Fi Direct) everything must go perfectly:
# these decide. Under 5 Mbit/s (degraded: last) it must only degrade
# gracefully: less quality, no stall.
grid="adsl 15mbit,15,0.2,200
deep 10mbit,20,0.1,300
shallow 15mbit,5,0.1,15
lossy 20mbit,5,2,50
long 20mbit,80,0.1,200
hotspot 50mbit,5,0.1,80
p2p 300mbit,1,0.05,20
steps 20mbit,15,0.2,200,steps
bursts 30mbit,5,0.2,60,bursts
cross 20mbit,10,0.1,200,cross
degraded 3mbit,10,0.5,300"
echo "$grid" | while read -r name profile; do
    [ -n "$name" ] || continue
    echo "######## $name ($profile)"
    tools/netbench.sh "$profile" "$runs" "$seconds" > "$logs/$name.log" 2>&1 || true
    grep -c "test=failed" "$logs/$name.log" | sed 's/^/failed runs: /'
    python3 -I tools/netbench-summary.py < "$logs/$name.log"
done
