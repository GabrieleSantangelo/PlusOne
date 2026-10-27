#!/bin/sh
# Everything CI checks, in order. Runs inside Dockerfile.ci.
set -e
cd "$(dirname "$0")/.."
# Hand the built package back to the host user when run from tools/ci-docker.sh.
[ -n "$HOST_UID" ] && trap 'chown -R "$HOST_UID:${HOST_GID:-$HOST_UID}" dist 2>/dev/null' EXIT
python3 -m compileall -q plusone tools tests
# The application and its window must at least import without a display.
python3 -c "import gi; gi.require_version('Gtk', '4.0'); import plusone.app, plusone.gui, plusone.sign"
# GUI tests need a display: a headless Weston (a Wayland compositor that
# draws into memory) stands in for the desktop.
export XDG_RUNTIME_DIR="$(mktemp -d)"
weston --backend=headless --socket=wayland-ci --idle-time=0 >/dev/null 2>&1 &
weston=$!
for _ in $(seq 50); do [ -S "$XDG_RUNTIME_DIR/wayland-ci" ] && break; sleep 0.1; done
GDK_BACKEND=wayland WAYLAND_DISPLAY=wayland-ci PLUSONE_GUI_TESTS=1 \
    python3 -m pytest -q -p no:cacheprovider tests
kill $weston
packaging/build-deb.sh
# This version's package only: ./dist may keep older ones.
version="$(sed -n 's/^__version__ = "\(.*\)"/\1/p' plusone/__init__.py)"
lintian --fail-on error "dist/plus-one_${version}_$(dpkg --print-architecture).deb"
