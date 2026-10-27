#!/bin/sh
# Run the CI checks locally in the same container GitHub uses.
# The built .deb lands in ./dist.
set -e
cd "$(dirname "$0")/.."
docker build -f Dockerfile.ci -t plus-one-ci .
mkdir -p dist
# The last commit's date for the package (the container has no .git).
# The link's program, for the package, and its test tablet, for
# tests/test_plusone_link.py as GitHub's link job runs it (their own
# container: tools/link-docker.sh).
tools/link-docker.sh build --release --workspace --examples --bins > /dev/null
docker run --rm -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" -e SOURCE_DATE_EPOCH="$(git log -1 --format=%ct)" \
    -e PLUSONE_LINK_BIN=link/target/release \
    -v "$PWD/dist:/src/dist" -v "$PWD/link/target/release:/src/link/target/release:ro" plus-one-ci
