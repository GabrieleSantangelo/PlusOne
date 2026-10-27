<p align="center">
  <img src="data/io.github.gabrielesantangelo.PlusOne.svg" width="128" alt="Plus One icon">
</p>

<h1 align="center">Plus One</h1>

<p align="center">
  <a href="https://github.com/GabrieleSantangelo/PlusOne/actions/workflows/ci.yml"><img src="https://github.com/GabrieleSantangelo/PlusOne/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/GabrieleSantangelo/PlusOne/releases/latest"><img src="https://img.shields.io/github/v/release/GabrieleSantangelo/PlusOne" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT"></a>
</p>

<p align="center">Your Samsung Galaxy Tab as a wireless second monitor for GNOME on Linux,<br>
a signing tablet, a webcam, a microphone and speakers, through Plus One's small app on it.</p>

---

## Features

- **Real extended display.** A virtual monitor appears in *Settings → Displays*;
  drag windows onto the tablet like onto any other screen.
- **Low latency, over your Wi-Fi or a cable.** About 25 ms from the PC's
  screen to the tablet's decoder at 1920×1200@60, hardware H.264 (Intel's
  low-power encoder first, then NVENC, VA-API) or x264, encrypted. The
  bitrate follows what the network takes, so a weak one makes the picture
  softer, not late. Plug the tablet in and it goes by the cable (USB), the
  fastest way; pull it out and the desktop goes on over the Wi-Fi, about
  two seconds later, without closing. Plus One and the tablet always say
  which way they are connected.
- **Touch and S Pen.** The fingers work as a laptop's touchpad, where they
  touch: tap, drag, two to scroll with momentum, pinch to zoom, three to
  swipe. The S Pen becomes a real Linux drawing tablet with pressure (4096
  levels), tilt and hover.
- **Only your tablet.** Pair it once in Plus One's window: both screens show
  the same code. From then on the two recognize each other, and nothing else
  on the network gets the desktop, the PDFs, the camera or the sound.
- **Sign PDFs on the tablet.** Right-click a PDF → *Sign on Tablet*: it opens
  in Plus One's app, you sign with the pen, ink right under the nib, and a
  signed copy appears next to the original.
- **One click.** A *Tablet* toggle in GNOME's Quick Settings, a notification
  with *Disconnect*, and Plus One ready in the background from login.
- **The tablet's camera and microphone for the PC.** A webcam and a
  microphone that every program sees (Chrome, Firefox, Discord, Slack,
  OBS...). The tablet turns them on only while a program uses them, so
  nothing goes over the network otherwise; each has its own switch.
- **The tablet's speakers for the PC.** A sound output, *Tablet speakers*,
  there while the tablet is: the sound goes as it is (16-bit stereo PCM at
  48 kHz, no codec), only while a program plays, in time with the picture.
- **Up to date by themselves.** The `.deb` updates through apt, and the PC
  brings the tablet's app to its own version.
- **Simple GNOME app**, in English and Italian.

## Requirements

| | |
|---|---|
| **PC** | Linux with GNOME 46+ on **Wayland** (Mutter), on the same network as the tablet. Tested on Ubuntu 24.04. |
| **Encoder** | One of: NVIDIA (NVENC), Intel/AMD (VA-API), or software x264. |
| **Tablet** | A Galaxy Tab with Android 13 or later, with Plus One's app (`plus-one-companion_*.apk`, in each release). Tested on a Galaxy Tab S10 FE+ with One UI 8.5. |

## Install

### Debian / Ubuntu package

Download `plus-one_<version>_all.deb` from the
[Releases](../../releases) page, then:

```bash
sudo apt install ./plus-one_0.8.1_all.deb
```

The package pulls in GTK 4, libadwaita and GStreamer from your distribution. It also installs the S Pen's helper, which makes the
pen a tablet with pressure and tilt: a small program that runs as root only
when Plus One asks (D-Bus activation), only for the session in front of the
PC (polkit), and makes the pen's virtual HID device and nothing else, taking
only pen reports. Nobody is given `/dev/uhid` itself, which could make a
virtual keyboard; there is no group to join and nothing to log out for.

The *Tablet* Quick Settings toggle (a GNOME Shell extension) is included but
off: after logging out and back in, turn it on in Plus One's window
(*General → Quick Settings toggle* in the window) or in the *Extensions* app. Plus One never
enables shell extensions by itself.

**Updates come by themselves.** Each release on GitHub is also a small APT
repository, signed with Plus One's key: the package installs its source
(`/etc/apt/sources.list.d/plus-one.sources`, with the key in
`/usr/share/keyrings/plus-one-archive-keyring.gpg`), so `apt upgrade` and
Software Updater see new versions, and unattended-upgrades installs them
once a day as it does Ubuntu's security updates
(`/etc/apt/apt.conf.d/52plus-one-unattended-upgrades`; remove it, or comment
its line out, to be asked instead). Plus One notices it was replaced and
starts again once nothing is in use, with a notification; *Restart now* does
it at once. Installing the `.deb` by hand is needed once, from 0.8.1 on.

Uninstall with `sudo apt remove plus-one` (`purge` also removes the source).

### Flatpak (any distribution with GNOME)

Download `plus-one_<version>_x86_64.flatpak` from the [Releases](../../releases)
page, then:

```bash
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak install --user ./plus-one_v0.8.1_x86_64.flatpak
```

The GNOME runtime it needs comes from Flathub. Differences from the `.deb`:

- A sandbox cannot install the S Pen's helper: for pressure and tilt, run
  `sudo ./enable-pen.sh` once from the sources (it installs the helper on the
  host); without it the pen works as a mouse.
- The sandbox is not much of a boundary: Plus One needs GNOME's screen
  capture and remote input without asking (the virtual monitor and the
  tablet's touches), the host's dconf (mapping the pen to the monitor) and
  the devices (`/dev/uhid`).
- The Quick Settings toggle is a GNOME Shell extension, which a Flatpak cannot
  install. Get `plus-one@gabrielesantangelo.github.io.shell-extension.zip` from
  the same release, then run
  `gnome-extensions install plus-one@gabrielesantangelo.github.io.shell-extension.zip`,
  log out and back in, and enable it in *Extensions*.
- Upgrading from 0.2: the app ID changed, so remove the old one with
  `flatpak uninstall io.github.santal.PlusOne`.

### From source

```bash
git clone https://github.com/GabrieleSantangelo/PlusOne.git plus-one && cd plus-one
./install.sh               # menu entry, icon, Quick Settings toggle for your user, no root
sudo ./enable-pen.sh       # optional: S Pen pressure and tilt (the pen's helper)
```

Dependencies on Ubuntu:

```bash
sudo apt install python3-gi python3-gi-cairo gir1.2-gtk-4.0 gir1.2-adw-1 gir1.2-gstreamer-1.0 \
    gir1.2-poppler-0.18 python3-pypdf python3-cryptography openssl avahi-daemon \
    gstreamer1.0-pipewire gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-vaapi gstreamer1.0-x \
    gstreamer1.0-libav gstreamer1.0-nice gir1.2-gst-plugins-bad-1.0 swh-plugins pulseaudio-utils pipewire-bin
```

(The `.deb`'s list, in `packaging/build-deb.sh`, is the reference.)

## Usage

Once: install Plus One's app on the tablet (`plus-one-companion_*.apk` from
the release, Android 13 or later) and open it. Then in Plus One's window on
the PC, **Pair a tablet**: pick yours from the list of tablets with the app
on your Wi-Fi, check that both screens show the same six-digit code and
confirm on both. The window's **Tablets** page (or a click on the tablet's
name at the top of the sidebar) pairs another, picks the one in use and
forgets one.

The app is then kept at the PC's version by the PC itself: when the app on
the tablet is older and nothing is in use, the PC hands it the app of its
own version (from its release, checked against `SHA256SUMS`; Android checks
that it is signed as the app it replaces), and the app installs it. The
first time Android asks, with one tap (the app did not install itself);
after that it updates without a question. *Tablets → Keep the tablet's app
up to date* turns it off.

1. Click **Tablet** in GNOME's Quick Settings (top-right corner), or press
   **Use as second screen** in the Plus One window.
2. That is all: the app on the tablet opens the desktop by itself, a new
   monitor to the right of your other screens (arrange it in *Settings →
   Displays*). The S Pen works on it as a pen tablet; the fingers as a
   laptop's touchpad, where they touch:

   | Fingers | |
   |---|---|
   | one tapped | click |
   | one moved | drag (select text, move a window) |
   | one held still | right button |
   | two tapped | right button |
   | two moved together | scroll; flicked, it goes on and slows down, until a finger touches |
   | two moved apart or together | pinch: zoom where the program does (browsers, maps, viewers) |
   | three swiped | up: the overview; down: out of it; sideways: another workspace |

Leaving the app for another one, or the tablet's screen going off, pauses
the desktop (the PC captures and encodes nothing meanwhile); its
notification, *Showing the desktop of …*, brings it back. To stop, Back on
the tablet, *Stop* in that notification, or *Disconnect* on the PC.

Plus One runs in the background from login (*Ready in the background*), so the
toggle answers at once; idle, it uses no radio and no CPU.

To connect from a keyboard shortcut, bind this command in *Settings → Keyboard
→ Custom Shortcuts* (`Disconnect` works the same way):

```bash
gdbus call --session -d io.github.gabrielesantangelo.PlusOne -o /io/github/gabrielesantangelo/PlusOne \
    -m io.github.gabrielesantangelo.PlusOne1.Connect
```

### Sign a PDF on the tablet

Right-click a PDF in Files → **Sign on Tablet** (or *Open With → Sign on
Tablet*, or launch *Sign on Tablet* from the app grid and pick a file). Files
offers it while a tablet is on your network or connected: Plus One sees it
when the tablet announces itself, which Android's Quick Share and KDE Connect
both do. **Signing → Sign on Tablet** in the Plus One window
turns that entry off (in Files, Open With and the app grid). The PDF goes to the tablet in use only, and only to
a paired one; **Sign a PDF…** in the window does the same.

1. The document opens in Plus One's app on the tablet, by itself if the app
   may display over other apps, else from its notification. Sign with the
   S Pen: it draws with pressure, right under the nib, and erases while you
   hold its side button; fingers never draw, so resting your hand is fine.
   Pinch with two fingers to zoom and move.
2. Tap **Done**. `name-signed.pdf` is saved next to the original (which is
   left untouched) and a notification offers *Open* and *Show in folder*.

The pen button in the toolbar sets the colour, the thickness and the
stabilization (how much the ink smooths the pen's movement); Plus One
remembers them. **My signature** places your saved signature with one tap;
its arrow lists them all: pick another, remove one, or draw a new one on a
pad (with the pen or the mouse). The first time, **Keep as my signature**
also saves what you just drew. **Signing → My signatures** in the Plus One
window manages them too. **Date** and **Text** add a date or a few words
where you tap; **Undo**, **Clear page**, page arrows and **Fit**.

What you place comes selected, in a blue box: drag it from inside, turn it
with the round handle, resize it from a corner; tap elsewhere when it is
right. The **lasso** button selects anything on the page the same way:
draw around it, or tap a stroke; **Delete** (or the Delete key) removes
the selection.
If the tablet's app cannot be reached, the notification offers *Sign on this
PC* (the same tools, with the mouse or a pen tablet). The signature is drawn into the page (a simple
electronic signature), not a cryptographic digital signature.

The Files menu entry needs `python3-nautilus` (installed with the `.deb` by
default) and appears after Files restarts (`nautilus -q`). In the Flatpak,
use *Open With*; if the folder cannot be written, Plus One asks where to save.

### Plus One's app on the tablet

1. Install `plus-one-companion_*.apk` from the release on the tablet
   (Android 13 or later; allow installing from your browser or file manager
   when Android asks), and open **Plus One** once.
2. In the app, allow it to **display over other apps**, so the desktop and
   a PDF sent from the PC open by themselves (without it, a notification to
   tap).
3. Pair it in Plus One's window on the PC (see *Usage*). From then on they
   recognize each other, and nothing else on the network can reach the
   tablet through it. Nothing ever pairs by itself.

The pen and the signatures are shared with the PC: each time you sign, what
you added or removed on either side reaches the other. The app lists them
too, under **My signatures**. It waits for the PC with a quiet notification;
**Ready in the background** in the app turns that off (it is then ready for
10 minutes after you open it). If the Wi-Fi drops while you sign, Plus One
finds the app again and gets the signature when you are done.

When the PC hands the app an update, Google Play Protect may ask to scan it
first (*App scan recommended*): *Scan app*, and it installs.

**More than one tablet.** Pair each of them; the one in use, picked under
its name in the window, is the one that gets the PDFs and the desktop.
Other tablets on the network, with the app or not, get nothing.

**Several pages.** The pages lie one under the other, as in a note app: a
finger scrolls through them, and the pen signs on whichever page it touches.
In the app, **Documents of several pages → Scroll through the pages** off
shows one page at a time, turned with the arrows at the top.

**Several PDFs at once.** Select them all in Files → *Sign on Tablet* (or
send them one at a time, also while you sign): they wait on the tablet in the
order you sent them. The first opens; *Done* (or ✕, to leave one unsigned)
sends it back to the PC and opens the next. The documents button in the
top bar lists them, to sign them in any order: what you drew on one stays
there while you look at another. The app's main screen lists them too, under
**To sign**, and so does a notification.

### The tablet's camera and microphone

Turn on **Tablet camera** and **Tablet microphone** in Plus One's window
(and, on the tablet, **For the PC → Camera / Microphone** in Plus One's app,
which asks Android's permission). The PC then has a webcam called *Plus One
tablet camera* and a microphone called *Tablet microphone (Plus One)*: pick
them in your video-call program like any other. The tablet captures only
while a program on the PC uses one of them (a call, the Camera app), and
stops a few seconds after it lets go; the switch off removes the device from
the PC altogether. While a switch is on, any program on this PC can use
it, as with a webcam plugged in: the window says which program does, and
the tablet says so in its notification. They go over **WebRTC**, as in a video call: Google's
libwebrtc on the tablet (the sound's gain control, congestion control,
retransmissions) and GStreamer's webrtcbin on the PC (the jitter buffer), encrypted
(DTLS-SRTP, its keys vouched for by the pairing), over your Wi-Fi; H.264 up to 2.5 Mbit/s at 720p (5 at 1080p), Opus for the sound,
about 160 ms from the tablet's microphone to a program on the PC (measured).
The PC needs `gstreamer1.0-nice`, `gir1.2-gst-plugins-bad-1.0` and
`swh-plugins` (the `.deb` brings them). The window's **Camera** page picks
the front or the back camera, 720p or 1080p, and its **Microphone** page the
noise reduction:

- **Neural, on this PC** (the default): [DeepFilterNet](https://github.com/Rikorose/DeepFilterNet),
  a neural network that takes the room's noise out of a voice (20 ms more,
  about half a processor core while a program records). Its plugin is not
  packaged; once, from its releases (MIT or Apache-2.0):

  ```bash
  mkdir -p ~/.ladspa && curl -fL -o ~/.ladspa/libdeep_filter_ladspa.so \
      https://github.com/Rikorose/DeepFilterNet/releases/download/v0.5.6/libdeep_filter_ladspa-0.5.6-x86_64-unknown-linux-gnu.so \
      && echo "2ca3205c2911d389604a826a240e745597d50252b5cab81c8248252b335e2236  $HOME/.ladspa/libdeep_filter_ladspa.so" \
      | sha256sum -c
  ```

  Without it, *Light* instead.
- **Light, on the tablet**: libwebrtc's noise suppression.
- **Off**.

Then, whichever: a compressor and a limiter (swh-plugins') bring the voice
to about -18 dBFS, as calls want, without its peaks going over -1 dBFS.

The webcam is a v4l2loopback device. The `.deb` sets it up; from source or
the Flatpak, once:

```bash
sudo cp data/plus-one-camera-options.conf /etc/modprobe.d/ && sudo cp data/plus-one-camera.conf /etc/modules-load.d/ \
    && sudo modprobe v4l2loopback
```

Ubuntu's recent kernels include the module; on older ones, `sudo apt install
v4l2loopback-dkms` first.

### The tablet's speakers

Turn on **Speakers → Tablet speakers** in Plus One's window: the PC's sound
goes to the tablet at once (the switch makes it the output; GNOME's Quick
Settings and Sound settings choose it too, as *Tablet speakers (Plus
One)*). The output is there only while the tablet's app is on the network:
away, GNOME plays on the PC again. While a program plays, the sound goes as
it is, 16-bit stereo PCM at 48 kHz in encrypted packets of 5 ms (1.5
Mbit/s), and the app plays it as the tablet's own players do, with the
tablet's media volume; nothing goes while nothing plays, nor after a
second of silence (a paused player that keeps its stream open). On the tablet,
**For the PC → Speakers** in Plus One's app (on at first) lets the PC use
them.

### Settings

The window has a sidebar of what Plus One does: **Second screen**,
**Signing**, **Camera**, **Microphone**, **Speakers**, **Tablets**,
**General**; each page has its button and all its settings in view (a dot
in the sidebar marks what is on, green while in use). Narrow, the sidebar
and the page take turns. Ctrl+, opens *General*. Plus One speaks the
system's language.

| Setting | |
|---|---|
| Second screen → Quality | *Balanced* (20 Mbit/s at 1920×1200, the default), *Sharper text* (40) or *For weak Wi-Fi* (10): the most, less while the network does not take it (frames lost, a queue growing), down to 1.5 Mbit/s. |
| Second screen → Resolution | *Automatic* (1920×1200 on a Tab S10 FE+) or the tablet's sizes at its proportions, from 2560 wide at scale 2 (sharp text). |
| Second screen → Touch and S Pen | The fingers and the pen from the tablet. |
| Tablets → Keep the tablet's app up to date | The PC hands the app its own version while nothing is in use. On by default. |
| General → Quick Settings toggle | Turns the *Tablet* GNOME Shell extension on or off (Plus One offers it once, after the first pairing). Shown when the extension is installed (the `.deb`, or the zip for Flatpak users) and GNOME Shell knows it, which is after the next login. |
| General → Use the cable | The tablet plugged in goes by the cable (Android asks once whether Plus One opens it). Its file transfer (MTP) is off while Plus One has the cable: turn this off to copy files by cable. On by default. |
| General → Ready in the background | Start at login and keep running after the window closes, so the Quick Settings toggle connects at once. Costs nothing while idle. |

### S Pen

The first time you bring the pen near the screen after connecting, Plus One
creates a virtual tablet called **Plus One S Pen** and maps it onto the tablet's
monitor; to Linux it is an ordinary drawing tablet, so apps with tablet support
get pressure and tilt. GIMP 2.10 runs through XWayland: restart it after the pen
has appeared, then in *Edit → Input Devices* set **xwayland-tablet stylus** to
*Screen*.

The virtual tablet comes from the pen's helper (the `.deb` installs it;
`sudo ./enable-pen.sh` from the sources). Without it the pen still works as a
mouse (hover moves the pointer, tip clicks, side button right-clicks).

### Command line

```bash
plus-one --cli                       # the desktop on the tablet in use, no window
plus-one --cli --size 2560x1600      # one of the tablet's sizes
plus-one --cli --test                # a test pattern instead of the desktop
plus-one --help
```

`python3 -m plusone.appdisplay ADDRESS --test --preview` shows the same
moving timer on the PC and on the tablet: photograph both screens together
and the difference between the two timers is the latency.

## Troubleshooting

| Problem | What to do |
|---|---|
| *“… is not on the network: open Plus One on it”* | Open Plus One's app on the tablet, on the same network as the PC (and keep *Ready in the background* on in the app). A guest network that keeps devices apart, or a VPN with a firewall or kill switch, can keep them from seeing each other. |
| *“… did not answer”* | The app was there a moment ago: open it on the tablet, or move closer. Open *General → Log* in the window. |
| *“… let Plus One display over other apps”* | In the app, allow it, or open Plus One on the tablet before connecting. |
| *“… no longer knows this PC”* | The app forgot this PC (reinstalled, or *Forget* on the tablet): pair it again from the window. |
| *“… are different versions”* | The PC updates the app by itself once nothing is in use (Play Protect may ask for a tap); else install the release's APK by hand. |
| *Tablet camera* says it needs v4l2loopback | See *The tablet's camera and microphone*: the module is not loaded. |
| The camera or microphone says the tablet does not allow it | In Plus One's app on the tablet, *For the PC*: turn it on, and allow it when Android asks. |
| Picture soft | The Wi-Fi does not take more: Plus One lowers the bitrate rather than letting the delay grow. Move closer, or use a 5 GHz network. The log says how much it takes. |
| Pen not in GIMP | See the S Pen section (restart GIMP, enable *xwayland-tablet stylus*). |
| App icon or Quick Settings toggle missing after install | Log out and back in; GNOME Shell only looks for new icons and extensions at login. |

## How it works

```
GNOME (Mutter)                  Plus One (PC)                          Plus One's app (tablet)
──────────────                  ─────────────                          ───────────────────────
virtual monitor ──PipeWire──▶ capture ▶ H.264 ──UDP, AES-GCM──▶ decoder ▶ screen
remote desktop  ◀── fingers, as a touchpad ── input ◀──────────────────── touch / S Pen
/dev/uhid       ◀── S Pen ──────────────────┘
                       TLS link (paired): keys, requests, PDFs, updates
```

- **Display**: `org.gnome.Mutter.ScreenCast.RecordVirtual` creates the monitor
  without a consent dialog, at the size asked for.
- **Link**: the app announces itself with mDNS (`_plusone._tcp`); the PC
  connects over TLS 1.3, both certificates pinned at pairing. The session's
  key goes over it; the frames, the input and the sound go over UDP, sealed
  with AES-GCM.
- **Picture**: each frame is sent the moment the encoder has it and shown as
  soon as it is decoded; the bitrate follows the losses and the delay the app
  reports each second, and a frame is not even encoded while the data before
  it still waits to go out.
- **Input**: the fingers' state comes in packets the app repeats; the PC
  turns it into clicks, drags, scrolling (with momentum), pinches and
  swipes through Mutter's remote desktop; the pen's reports are replayed into
  `/dev/uhid` by the pen's helper.
- **Application**: one GApplication process holds the connection and offers it
  over D-Bus (`io.github.gabrielesantangelo.PlusOne1`: `Connect`, `Disconnect`,
  `ShowWindow`, `SignDocument(path)`, properties `State`, `Active`, `Title`, `Detail`, `Background`).
  The window, the notifications and the Quick Settings extension are front
  ends; D-Bus activation starts the process when one of them needs it.

Up to 0.8 Plus One could also use Samsung's *Second Screen* (Miracast over
Wi-Fi Direct); the reverse engineering behind it is in
[docs/FINDINGS.md](docs/FINDINGS.md).

What comes next, and the convenience rules every feature follows, is in
[docs/ROADMAP.md](docs/ROADMAP.md).

## Development

```
plusone/link/     connectivity, under every service: discovery (discovery.py, from every source;
                  nearby.py: mDNS), trust and pairing (trust.py), the tablets (tablets.py),
                  transports, links (link.py: connect, request, hold), datagrams (sealed UDP)
plusone/          the services and the app: app.py (background app, D-Bus, notifications), gui.py (window),
                  engine.py + appdisplay.py + media.py + display.py + pen.py (the desktop on the tablet),
                  signing.py + sign_jobs.py + sign.py + pdf.py + ink.py + modeler.py (Sign on Tablet),
                  devices.py + rtc.py + pcm.py (the tablet's camera, microphone and speakers),
                  updates.py (both sides up to date)
data/gnome-shell/ the Quick Settings extension
data/nautilus/    the Files context-menu entry
android/          the companion app for the tablet (Kotlin): link/ (listeners, announcers, pairing,
                  the PCs' links and their requests, datagrams; its tests in app/src/test) under
                  CompanionService and its services, the *Handler.kt
tests/            unit and protocol tests (a scripted fake tablet, no hardware needed)
packaging/        build-deb.sh and maintainer scripts
tools/            CI scripts; tools/re/ has the reverse-engineering helpers (the Second Screen ones, and its measurements, are in the history up to 0.8)
docs/             the roadmap; FINDINGS.md, the reverse engineering of Second Screen (up to 0.8)
```

Run the app from the checkout with `bin/plus-one`.

Tests:

```bash
python3 -m pytest tests          # quick, on the host
tools/ci-docker.sh               # everything CI runs, in an Ubuntu 24.04 container
```

The companion app builds in a container too, so no Android SDK goes on the
host: `tools/android-docker.sh` (Gradle tasks as arguments, `assembleDebug`
by default) puts the APK in `android/app/build/outputs/apk/`; install it with
`adb install -r`. Building `Dockerfile.android` accepts the Android SDK
license.

`tools/ci-docker.sh` builds `Dockerfile.ci`, runs the tests, builds the `.deb`
into `dist/` and checks it with lintian. GitHub Actions runs the same container
on every push and pull request, and also builds the Flatpak.

Flatpak, locally:

```bash
flatpak install --user flathub org.flatpak.Builder org.gnome.Sdk//50
flatpak run org.flatpak.Builder --user --install --force-clean flatpak-build \
    packaging/flatpak/io.github.gabrielesantangelo.PlusOne.yml
```

### Releasing

1. Bump `__version__` in `plusone/__init__.py` and add a section to
   `CHANGELOG.md`.
2. Commit, then tag and push: `git tag v0.8.1 && git push origin main v0.8.1`.
3. The *Release* workflow checks that the tag matches the version, runs the
   tests, builds the `.deb` and the Flatpak bundle and publishes a GitHub
   release with both, the Quick Settings extension zip and the changelog
   section as notes. No tests, no release.
4. With the secrets `ANDROID_KEYSTORE` (the release key, a PKCS#12 file in
   base64) and `ANDROID_KEYSTORE_PASSWORD`, it also builds the companion app,
   signed, and attaches the APK. The app's version is Plus One's.
5. With the secret `APT_SIGNING_KEY` (the armored secret key whose public
   half is `packaging/apt/plus-one-archive-keyring.gpg`), it makes the
   release an APT repository (`packaging/apt/make-repo.sh`: `Packages`,
   `Release`, `InRelease`, signed) and publishes it only once everything is
   attached, so `releases/latest/download/` is always a whole one.

## Acknowledgements

- [GNOME Network Displays](https://gitlab.gnome.org/GNOME/gnome-network-displays),
  the reference for Wi-Fi Direct and WFD on GNOME, when Plus One went through
  Second Screen.
- [segunda-tela-linux](https://github.com/paulorochagodoi/segunda-tela-linux),
  whose notes on the Windows side of Second Screen were the starting point.
- [REA](https://github.com/morluto/rea) and JADX, used to study the tablet's
  receiver.

## Disclaimer

Plus One is an independent project, not affiliated with or endorsed by
Samsung. Samsung, Galaxy and Second Screen are trademarks of Samsung
Electronics. No Samsung code or binaries are included.

## License

[MIT](LICENSE)
