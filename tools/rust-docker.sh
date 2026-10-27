#!/bin/sh
# Plus One in Rust (rust/), built in the Dockerfile.rust container: the
# toolchain and the -dev libraries never touch the host.
#   tools/rust-docker.sh fetch        crates.io's crates, as Cargo.lock pins
#                                     them (downloaded only: nothing runs)
#   tools/rust-docker.sh build|test|clippy|... [args]
#                                     cargo, with no network at all: what the
#                                     crates' build scripts and tests run
#                                     cannot download or send anything
# The crates stay in the plus-one-cargo volume; the binary lands in
# rust/target/release, for this Ubuntu (24.04).
set -e
cd "$(dirname "$0")/.."
docker build -q -t plus-one-rust - < Dockerfile.rust > /dev/null
command="${1:-build}"
shift || true
if [ "$command" = fetch ]; then
    network=""
    cargo="cargo fetch --locked"
    [ -f rust/Cargo.lock ] || cargo="cargo generate-lockfile && cargo fetch --locked"
else
    network="--network none"
    cargo="cargo $command --offline --locked $*"
fi
docker run --rm $network -v "$PWD:/src" -v plus-one-cargo:/cargo plus-one-rust \
    sh -c "$cargo; status=\$?; chown -R $(id -u):$(id -g) /src/rust; exit \$status"
