#!/bin/sh
# Build dist/plus-one_<version>_<arch>.deb from this checkout (no root
# needed). The link's program, plus-one-link, built before (tools/link-docker.sh
# build --release, or PLUSONE_LINK_PROGRAM): the package is this machine's
# architecture's.
set -e
umask 022
here="$(dirname "$(readlink -f "$0")")"
top="$(dirname "$here")"
version="$(sed -n 's/^__version__ = "\(.*\)"/\1/p' "$top/plusone/__init__.py")"
app_id=io.github.gabrielesantangelo.PlusOne
pkg=plus-one
maintainer="santal1210 <sant.nino02@gmail.com>"
arch="$(dpkg --print-architecture)"
link_bin="${PLUSONE_LINK_PROGRAM:-$top/link/target/release/plus-one-link}"
[ -x "$link_bin" ] || { echo "no $link_bin: tools/link-docker.sh build --release first" >&2; exit 1; }
root="$top/build/deb/${pkg}_${version}_${arch}"

rm -rf "$root"
install -d "$root/DEBIAN" "$root/usr/bin" "$root/usr/lib/$pkg" \
    "$root/usr/share/applications" "$root/usr/share/icons/hicolor/scalable/apps" \
    "$root/usr/share/metainfo" "$root/usr/share/doc/$pkg" "$root/usr/share/dbus-1/services" \
    "$root/usr/lib/modules-load.d" "$root/usr/lib/modprobe.d" \
    "$root/usr/share/dbus-1/system-services" "$root/usr/share/dbus-1/system.d" "$root/usr/share/polkit-1/actions" \
    "$root/usr/lib/systemd/user"

cp -r "$top/plusone" "$top/plusone_link" "$root/usr/lib/$pkg/"
find "$root/usr/lib/$pkg" -name __pycache__ -prune -exec rm -rf {} +
chmod -R u=rwX,go=rX "$root/usr/lib/$pkg"
cat > "$root/usr/bin/$pkg" <<LAUNCHER
#!/bin/sh
# -P: not the current directory's files first (a json.py in ~/Downloads).
PYTHONPATH="/usr/lib/$pkg\${PYTHONPATH:+:\$PYTHONPATH}" exec python3 -P -m plusone "\$@"
LAUNCHER
chmod 755 "$root/usr/bin/$pkg"

install -m 644 "$top/data/$app_id.desktop" "$top/data/$app_id.Sign.desktop" "$root/usr/share/applications/"
install -m 644 "$top/data/$app_id.svg" "$root/usr/share/icons/hicolor/scalable/apps/"
install -m 644 "$top/data/$app_id.metainfo.xml" "$root/usr/share/metainfo/"
install -m 644 "$top/data/$app_id.service" "$root/usr/share/dbus-1/services/"
# The link (docs/LINK.md): its program, started by its socket for each user
# (enabled for all by postinst; Plus One starts it in a session already open).
install -m 755 "$link_bin" "$root/usr/lib/$pkg/plus-one-link"
install -m 644 "$top/data/systemd/plus-one-link.socket" "$top/data/systemd/plus-one-link.service" "$root/usr/lib/systemd/user/"
# "Sign on Tablet" in Nautilus' context menu (needs python3-nautilus).
install -Dm 644 "$top/data/nautilus/plus-one-sign.py" "$root/usr/share/nautilus-python/extensions/plus-one-sign.py"
# Quick Settings toggle, off until the user turns it on.
install -d "$root/usr/share/gnome-shell/extensions"
cp -r "$top/data/gnome-shell/." "$root/usr/share/gnome-shell/extensions/"
chmod -R u=rwX,go=rX "$root/usr/share/gnome-shell"
# S Pen as a real tablet: a helper (root, D-Bus activated) makes the pen's
# virtual HID device for the session in front of the PC (polkit), and takes
# only pen reports; nobody gets /dev/uhid itself (which could make keyboards).
install -m 755 "$top/data/pen-helper/plus-one-pen-helper" "$root/usr/lib/$pkg/pen-helper"
install -m 644 "$top/data/pen-helper/io.github.gabrielesantangelo.PlusOne.Pen.service" \
    "$root/usr/share/dbus-1/system-services/"
install -m 644 "$top/data/pen-helper/io.github.gabrielesantangelo.PlusOne.Pen.conf" "$root/usr/share/dbus-1/system.d/"
install -m 644 "$top/data/pen-helper/io.github.gabrielesantangelo.plusone.policy" "$root/usr/share/polkit-1/actions/"
install -m 644 "$top/data/$pkg-uhid.conf" "$root/usr/lib/modules-load.d/"
# The tablet's camera as a webcam: a v4l2loopback device, loaded at boot.
install -m 644 "$top/data/$pkg-camera.conf" "$root/usr/lib/modules-load.d/"
install -m 644 "$top/data/$pkg-camera-options.conf" "$root/usr/lib/modprobe.d/$pkg-camera.conf"

# Updates: each GitHub release is an APT repository signed with this key;
# apt and Software Updater find new versions, unattended-upgrades installs
# them. Conffiles: the user may change or remove them, and purge does.
install -Dm 644 "$here/apt/plus-one-archive-keyring.gpg" "$root/usr/share/keyrings/plus-one-archive-keyring.gpg"
install -Dm 644 "$here/apt/plus-one.sources" "$root/etc/apt/sources.list.d/plus-one.sources"
install -Dm 644 "$here/apt/52plus-one-unattended-upgrades" "$root/etc/apt/apt.conf.d/52plus-one-unattended-upgrades"
printf '%s\n' /etc/apt/sources.list.d/plus-one.sources /etc/apt/apt.conf.d/52plus-one-unattended-upgrades \
    > "$root/DEBIAN/conffiles"
install -Dm 644 "$here/apt/lintian-overrides" "$root/usr/share/lintian/overrides/$pkg"

install -m 644 "$top/README.md" "$root/usr/share/doc/$pkg/README.md"
{
    echo "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/"
    echo "Upstream-Name: $pkg"
    echo
    echo "Files: *"
    echo "Copyright: 2026 santal1210"
    echo "License: MIT"
    sed 's/^$/./; s/^/ /' "$top/LICENSE"
} > "$root/usr/share/doc/$pkg/copyright"
{
    echo "$pkg ($version) unstable; urgency=medium"
    echo
    echo "  * Release $version."
    echo
    # The last commit's date, not the build's: the same sources make the same package.
    echo " -- $maintainer  $(date -R -d "@${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || date +%s)}")"
} | gzip -9n > "$root/usr/share/doc/$pkg/changelog.gz"
chmod 644 "$root/usr/share/doc/$pkg/"*

install -m 755 "$here/debian/postinst" "$here/debian/postrm" "$root/DEBIAN/"
size="$(du -sk --exclude=DEBIAN "$root" | cut -f1)"
# What plus-one-link links against (glibc...), as dpkg-shlibdeps says.
shlibs="$(tmp="$(mktemp -d)" && mkdir "$tmp/debian" && printf 'Source: %s\n\nPackage: %s\nArchitecture: any\n' "$pkg" "$pkg" > "$tmp/debian/control" \
    && cd "$tmp" && dpkg-shlibdeps -O -e "$root/usr/lib/$pkg/plus-one-link" 2>/dev/null | sed -n 's/^shlibs:Depends=//p'; rm -rf "$tmp")"
cat > "$root/DEBIAN/control" <<CONTROL
Package: $pkg
Version: $version
Section: utils
Priority: optional
Architecture: $arch
Maintainer: $maintainer
Installed-Size: $size
Depends: ${shlibs:+$shlibs, }python3 (>= 3.11), python3-gi, python3-gi-cairo, gir1.2-gtk-4.0, gir1.2-adw-1 (>= 1.5), gir1.2-gstreamer-1.0, gir1.2-poppler-0.18, python3-pypdf, python3-cryptography, openssl, gstreamer1.0-pipewire, gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-plugins-bad, gstreamer1.0-libav, gstreamer1.0-nice, gir1.2-gst-plugins-bad-1.0, swh-plugins, pulseaudio-utils, pipewire-bin, avahi-daemon, kmod, udev, acl, dbus, polkitd | policykit-1
Recommends: gstreamer1.0-vaapi, gstreamer1.0-plugins-ugly, gstreamer1.0-x, gnome-shell (>= 45), python3-nautilus
Homepage: https://github.com/GabrieleSantangelo/PlusOne
Description: Samsung Galaxy Tab as a second screen, signing tablet and more
 With Plus One's app on the tablet: the GNOME desktop extended onto it over
 the Wi-Fi or the cable, moving between them as they come and go (Mutter
 virtual monitor, low-latency H.264, the fingers as a touchpad, the S Pen
 with pressure and tilt), "Sign on Tablet" for PDFs from Nautilus or Open
 With, and the tablet's camera, microphone and speakers as the PC's.
CONTROL

mkdir -p "$top/dist"
dpkg-deb --root-owner-group -Zxz --build "$root" "$top/dist/${pkg}_${version}_${arch}.deb"
