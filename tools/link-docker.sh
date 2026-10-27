#!/bin/sh
# The link (link/), built in the Dockerfile.link container: the toolchain
# never touches the host.
#   tools/link-docker.sh fetch        crates.io's crates, as Cargo.lock pins
#                                     them, a new one's added (downloaded
#                                     only: nothing runs)
#   tools/link-docker.sh lock         Cargo.lock again after a crate of ours
#                                     came or went (no network: nothing new)
#   tools/link-docker.sh android      the app's JNI library (link/android),
#                                     into android/app/src/main/jniLibs, in
#                                     the Dockerfile.android container (its link-android stage)
#   tools/link-docker.sh build|test|clippy|... [args]
#                                     cargo, with no network at all: what the
#                                     crates' build scripts and tests run
#                                     cannot download or send anything
# The crates stay in the plus-one-cargo volume; the binaries land in
# link/target, for this Ubuntu (24.04).
set -e
cd "$(dirname "$0")/.."
command="${1:-build}"
shift || true
if [ "$command" = android ]; then
    if [ -z "$PLUSONE_IMAGE_BUILT" ]; then  # CI builds them, with its cache
        docker build -q --target android -t plus-one-android - < Dockerfile.android > /dev/null
        docker build -q --target link-android -t plus-one-link-android - < Dockerfile.android > /dev/null
    fi
    out=android/app/src/main/jniLibs/arm64-v8a
    docker run --rm --network none -v "$PWD:/src" -v plus-one-cargo:/cargo plus-one-link-android \
        sh -c "mkdir -p /src/$out && cargo build --offline --locked --release --target aarch64-linux-android -p plus-one-link-android \
               && cp target/aarch64-linux-android/release/libplusonelink.so /src/$out/; \
               status=\$?; chown -R $(id -u):$(id -g) /src/link /src/android/app/src/main/jniLibs; exit \$status"
    exit $?
fi
docker build -q -t plus-one-link - < Dockerfile.link > /dev/null
if [ "$command" = fetch ]; then
    network=""
    cargo="cargo fetch"
    [ -f link/Cargo.lock ] || cargo="cargo generate-lockfile && cargo fetch --locked"
elif [ "$command" = lock ]; then
    network="--network none"
    cargo="cargo update --workspace --offline"
else
    network="--network none"
    cargo="cargo $command --offline --locked $*"
fi
docker run --rm $network -v "$PWD:/src" -v plus-one-cargo:/cargo plus-one-link \
    sh -c "$cargo; status=\$?; chown -R $(id -u):$(id -g) /src/link; exit \$status"
