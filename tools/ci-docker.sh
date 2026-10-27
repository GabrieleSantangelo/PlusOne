#!/bin/sh
# Run the CI checks locally in the same container GitHub uses.
# The built .deb lands in ./dist.
set -e
cd "$(dirname "$0")/.."
docker build -f Dockerfile.ci -t plus-one-ci .
mkdir -p dist
# The last commit's date for the package (the container has no .git).
# The link's program, for the package (its own container: tools/link-docker.sh).
tools/link-docker.sh build --release -p plus-one-link-daemon > /dev/null
docker run --rm -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" -e SOURCE_DATE_EPOCH="$(git log -1 --format=%ct)" \
    -v "$PWD/dist:/src/dist" -v "$PWD/link/target/release/plus-one-link:/src/link/target/release/plus-one-link:ro" plus-one-ci
