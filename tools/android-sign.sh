#!/bin/sh
# Sign an APK built without the key (assembleRelease with no
# PLUSONE_KEYSTORE): apksigner in the Dockerfile.android container, the key
# file mounted read only and its password from the environment. Nothing of
# the build (Gradle, its plugins, the SDK's downloads) runs with the key.
#   PLUSONE_KEYSTORE=... PLUSONE_KEYSTORE_PASSWORD=... tools/android-sign.sh IN.apk OUT.apk
set -e
cd "$(dirname "$0")/.."
[ $# -eq 2 ] && [ -n "$PLUSONE_KEYSTORE" ] && [ -n "$PLUSONE_KEYSTORE_PASSWORD" ] || {
    echo "usage: PLUSONE_KEYSTORE=... PLUSONE_KEYSTORE_PASSWORD=... $0 IN.apk OUT.apk" >&2; exit 2; }
docker build -q --target android -t plus-one-android - < Dockerfile.android > /dev/null
key="$(cd "$(dirname "$PLUSONE_KEYSTORE")" && pwd)/$(basename "$PLUSONE_KEYSTORE")"
in="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
out_dir="$(mkdir -p "$(dirname "$2")" && cd "$(dirname "$2")" && pwd)"
out="$(basename "$2")"
docker run --rm -v "$key:/keys/release.p12:ro" -v "$in:/in.apk:ro" -v "$out_dir:/out" \
    -e PLUSONE_KEYSTORE_PASSWORD -e PLUSONE_KEY_ALIAS plus-one-android sh -c '
        tools="$ANDROID_HOME/build-tools/36.0.0"
        "$tools/zipalign" -p -f 4 /in.apk /tmp/aligned.apk
        "$tools/apksigner" sign --ks /keys/release.p12 --ks-type PKCS12 --v4-signing-enabled false \
            --ks-key-alias "${PLUSONE_KEY_ALIAS:-plusone}" --ks-pass env:PLUSONE_KEYSTORE_PASSWORD \
            --out "/out/'"$out"'" /tmp/aligned.apk
        "$tools/apksigner" verify "/out/'"$out"'"
        chown '"$(id -u):$(id -g)"' "/out/'"$out"'"'
