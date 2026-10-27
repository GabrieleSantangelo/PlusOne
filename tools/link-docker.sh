#!/bin/sh
# The link (link/), built in the Dockerfile.link container: the toolchain
# never touches the host.
#   tools/link-docker.sh fetch        crates.io's crates, as Cargo.lock pins
#                                     them (downloaded only: nothing runs)
#   tools/link-docker.sh build|test|clippy|... [args]
#                                     cargo, with no network at all: what the
#                                     crates' build scripts and tests run
#                                     cannot download or send anything
# The crates stay in the plus-one-cargo volume; the binaries land in
# link/target, for this Ubuntu (24.04).
set -e
cd "$(dirname "$0")/.."
docker build -q -t plus-one-link - < Dockerfile.link > /dev/null
command="${1:-build}"
shift || true
if [ "$command" = fetch ]; then
    network=""
    cargo="cargo fetch --locked"
    [ -f link/Cargo.lock ] || cargo="cargo generate-lockfile && cargo fetch --locked"
else
    network="--network none"
    cargo="cargo $command --offline --locked $*"
fi
docker run --rm $network -v "$PWD:/src" -v plus-one-cargo:/cargo plus-one-link \
    sh -c "$cargo; status=\$?; chown -R $(id -u):$(id -g) /src/link; exit \$status"
