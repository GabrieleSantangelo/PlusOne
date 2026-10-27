#!/bin/sh
# Build the companion app (android/) in the Dockerfile.android container:
# JDK, Android SDK and Gradle never touch the host. Gradle's caches stay in
# the plus-one-android-gradle volume (or the folder PLUSONE_GRADLE_CACHE, an
# absolute path: CI caches it). PLUSONE_IMAGE_BUILT=1: the container image is
# there already (CI builds it with its layers cached). Arguments are Gradle tasks
# (default: assembleDebug); the APK lands in android/app/build/outputs/apk.
# For a signed release (assembleRelease), set PLUSONE_KEYSTORE (a path on
# the host), PLUSONE_KEYSTORE_PASSWORD and optionally PLUSONE_KEY_ALIAS.
set -e
cd "$(dirname "$0")/.."
[ -n "$PLUSONE_IMAGE_BUILT" ] || docker build -q --target android -t plus-one-android - < Dockerfile.android > /dev/null
gradle="${PLUSONE_GRADLE_CACHE:-plus-one-android-gradle}"
owned="/src/android"
[ -z "$PLUSONE_GRADLE_CACHE" ] || owned="$owned /gradle"  # a folder of the host's: the user's again
tasks="${*:-assembleDebug}"
signing=""
if [ -n "$PLUSONE_KEYSTORE" ]; then
    # The key file alone, read only: not the folder it is in.
    key="$(cd "$(dirname "$PLUSONE_KEYSTORE")" && pwd)/$(basename "$PLUSONE_KEYSTORE")"
    signing="-v $key:/keys/release.p12:ro -e PLUSONE_KEYSTORE=/keys/release.p12
             -e PLUSONE_KEYSTORE_PASSWORD -e PLUSONE_KEY_ALIAS"
fi
# shellcheck disable=SC2086 # $signing is a list of options
docker run --rm -v "$PWD:/src" -v "$gradle:/gradle" $signing plus-one-android \
    sh -c "gradle --no-daemon --console=plain $tasks; status=\$?; chown -R $(id -u):$(id -g) $owned; exit \$status"
