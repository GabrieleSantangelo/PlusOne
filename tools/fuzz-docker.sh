#!/bin/sh
# The link's fuzz targets (link/fuzz/fuzz_targets) in the Dockerfile.fuzz
# container, with no network: what the parsers read comes from libFuzzer.
# A crash's input lands in link/fuzz/artifacts; the corpus grows in
# link/fuzz/corpus (neither is kept in git).
#   tools/fuzz-docker.sh fetch                  its crates (downloaded only)
#   tools/fuzz-docker.sh [SECONDS] [TARGET...]  each target SECONDS (600):
#                                               datagram frames mdns controller
set -e
cd "$(dirname "$0")/.."
docker build -q -t plus-one-fuzz - < Dockerfile.fuzz > /dev/null
owner="$(id -u):$(id -g)"
if [ "${1:-}" = fetch ]; then
    docker run --rm -v "$PWD:/src" -v plus-one-cargo:/cargo plus-one-fuzz \
        sh -c "cargo fetch --manifest-path fuzz/Cargo.toml; status=\$?; chown -R $owner /src/link/fuzz; exit \$status"
    exit $?
fi
seconds="${1:-600}"
shift || true
targets="${*:-datagram frames mdns controller}"
docker run --rm --network none -e CARGO_NET_OFFLINE=true -v "$PWD:/src" -v plus-one-cargo:/cargo plus-one-fuzz \
    sh -c "status=0
           for target in $targets; do
               echo \"== \$target, $seconds s\"
               cargo fuzz run --fuzz-dir fuzz --sanitizer none \$target -- -max_total_time=$seconds -print_final_stats=1 \
                   2>&1 | grep -E '^(==|#[0-9]+.*DONE|stat::number_of_executed_units|stat::peak_rss|.*(panicked|ERROR|SUMMARY|crash-|Test unit written))' \
                   || true
               [ -z \"\$(ls fuzz/artifacts/\$target 2>/dev/null)\" ] || status=1
           done
           chown -R $owner /src/link/fuzz
           exit \$status"
