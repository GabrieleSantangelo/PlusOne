# Plus One roadmap

The goal: the tablet becomes part of the Linux desktop like any other
peripheral. Its screen, touch and pen, but also its camera, microphone and
speakers (usually better than a laptop's) show up where GNOME already lists
such devices, and are used only when you pick them.

## The rule: convenience first

Being able to use the tablet is not enough; every task has to be quick to
start from wherever the user already is. Signing one PDF must not mean
"open Plus One, start the extended display, find a PDF app that handles a pen,
open the file, sign, save, close everything".

Every feature is designed and accepted against these rules:

1. **Start where you are.** Tasks begin from the file manager, GNOME Quick
   Settings, a notification or the tablet itself, never from "open Plus One
   first".
2. **At most one tap on the tablet**, and no choosing apps. Today that tap is
   the *Second screen* tile; the research track aims at zero.
3. **Connections are on demand.** A task opens the link it needs and closes it
   when done, unless the user is using the tablet as a monitor anyway.
4. **Results come back by themselves**: saved next to the original (never
   overwriting it) with a notification offering *Open* and *Show in folder*.
5. **Same features in the .deb and the Flatpak**, or a documented equivalent.
6. **Defaults that just work**; settings are for preferences, not for setup.

Each milestone states its acceptance target as *clicks on the PC / taps on
the tablet / waiting time*.

## Architecture change: a background service

Convenience needs something that is always there and that every entry point
can call. Plus One becomes a small session service plus thin front ends:

```
 Nautilus menu ─┐                                ┌─ virtual monitor (Mutter)
 Quick Settings ┼─ D-Bus ─▶ plus-one service ────┼─ Wi-Fi Direct + WFD session
 notifications ─┤   io.github.…PlusOne1          ├─ signing window on the tablet
 window ────────┘                                └─ Quick Share sender/receiver
```

- **D-Bus API** (`io.github.<id>.PlusOne1`): `Extend()`, `Mirror(connector)`,
  `TabletMode(connector)`, `Stop()`, `SignDocument(path) → path`,
  `SendFiles(paths)`; properties `State`, `Mode`, `Tablet`; change signals.
- **D-Bus activation and autostart** (XDG autostart for the .deb, the
  Background portal for the Flatpak). Idle cost: a sleeping process; Wi-Fi
  Direct is only touched when a task needs it, because searching for peers
  delays other Wi-Fi traffic (measured during a find: median unchanged, p90
  8 → 66 ms, p99 12 → 136 ms, 20% of packets over 30 ms).
- **Leases on the link**: every task holds one; the link closes when the last
  lease goes, unless the user pinned *Extend*.
- The current window becomes the settings and status page of the service.

## Milestones

### 0.3 — Always ready (done)

- Background application, D-Bus API, activation, autostart; the window is a
  front end like the others.
- **GNOME Shell Quick Settings toggle "Tablet"**: connect/disconnect, state as
  subtitle, menu with *Ready in the background* and *Settings*. *Mirror* and
  *Graphics tablet* join the menu in 0.5.
- Notifications: "Connected" with *Disconnect*, "tablet not found", errors.
- Searching is time-boxed (see the measurement above); reopening Second Screen
  within 90 s of closing it reconnects without a click.
- App ID moved to `io.github.gabrielesantangelo.PlusOne`.

Acceptance: from a fresh login, tablet as a monitor = 1 click + 1 tap.

### 0.4 — Sign on the tablet (done)

- **Entry points**: Nautilus *Sign on tablet* (nautilus-python, .deb),
  *Open With → Sign on tablet* (both packages), `SignDocument` over D-Bus.
- **Flow**: opens a temporary display if none is active; a signing window
  appears full screen on the tablet by itself (no app to pick); the PC screen
  shows a small "Signing on the tablet…" notice.
- **Signing window, built for the pen**:
  - pressure-sensitive ink;
  - fingers scroll and zoom, and touches are ignored while the pen is near
    (palm rejection);
  - *Place signature* reuses saved signatures (drawn once, stored as strokes);
  - text and date fields for forms;
  - large *Done* / *Cancel* buttons.
- **Result**: `name-signed.pdf` next to the original plus a notification;
  the temporary display closes.
- Tech: poppler-glib for rendering, ink and stamp annotations and saving (new
  dependency; the Flatpak builds it).
- Scope note: a drawn signature is a simple electronic signature. A digital
  PAdES signature with the user's certificate is a possible later addition.

Acceptance: right-click → *Sign on tablet* → 1 tap → sign → *Done*: no other
step, less than 10 s of overhead.

### 0.5 — Tablet modes and picture quality

- **Mirror** an existing monitor (presentations, teaching).
- **Graphics tablet**: the pen drives a chosen PC monitor like a Wacom while
  the tablet mirrors it; switch from Quick Settings.
- **2560×1600** through WFD R2 for sharper text (VESA index 33 is in the
  tablet's table).
- **Portrait**, if the sink accepts it (research `wfd_sec_portrait_display` and
  the 7260 side channel).
- **Adaptive bitrate** from the RTCP loss reports the tablet already sends.
- Optional **audio on the tablet through Second Screen**, no app needed: the
  tablet appears as a speaker in GNOME Sound; picking it reconnects with an
  audio track. Video latency goes from ~50 to ~90 ms (the sink syncs video to
  audio), so this suits films more than desktop work. The companion app (0.7)
  removes that cost.

### 0.6 — Quick Share

- Our own Python implementation of the Quick Share (Nearby Share) protocol:
  mDNS, UKEY2 handshake, protobuf frames, AES-CBC and HMAC. Written from the
  public protocol notes (NearDrop, RQuickShare), MIT, no extra program.
  Dependencies: `cryptography`, `protobuf`.
- Works on the home Wi-Fi; check whether it also runs over the Wi-Fi Direct
  link while that is up.
- **Send**: Nautilus *Send to tablet*, Quick Settings *Send files…*.
- **Receive**: the tablet's normal *Share → Quick Share → this PC*; files land
  in Downloads with a notification; auto-accepted from our paired tablet.
- **Sign with a tablet app**: the PC sends the PDF, you sign it in Samsung
  Notes (best palm rejection) and share it back; Plus One recognises it and
  saves `name-signed.pdf` next to the original.

Acceptance: send = right-click → *Send to tablet*; receive = share on the
tablet → notification on the PC.

### 0.7 — Companion app: camera, microphone, speakers

Second Screen only carries the picture, touch, pen and (at a latency cost)
audio. The rest of the tablet's hardware needs a small Plus One app on the
tablet: optional, installed once, and it stays out of the way.

- **Camera as a webcam**: front or rear camera, encoded on the tablet
  (MediaCodec H.264), decoded on the PC and published as a PipeWire camera,
  so it appears in GNOME Snapshot, Firefox, OBS, browser meetings. For apps
  that only know V4L2 (some Chrome/Zoom builds) an optional v4l2loopback
  device, set up once like the pen permission.
- **Microphone**: a PipeWire input "Tablet microphone".
- **Speakers**: a PipeWire output "Tablet speakers" with its own low-latency
  audio stream (Opus/PCM), independent of the video, so it does not slow the
  display down.
- **On demand, one tap at most**: the devices are always listed on the PC;
  when an app starts using one, Plus One wakes the companion, which shows one
  notification on the tablet (Android needs a visible start for camera and
  microphone). Nothing runs while unused.
- **Transport**: the Wi-Fi Direct link when the display is connected, else the
  home Wi-Fi (mDNS); encrypted and paired once with a code.
- **Install**: the APK is in the GitHub release, sent to the tablet with
  Plus One's own *Send to tablet* (0.6); no developer options.
- Kotlin, no Google services; built and attached to releases by CI.

Acceptance: in a meeting app pick "Tablet camera" → 1 tap on the tablet →
video, nothing else to start.

### 0.8 — Polish

- Optional USB mode through adb and the MS-MICE path (steadier link for
  people with a free port).
- Flathub submission.
- Documentation, screenshots, translations.

### Research track (in parallel)

- **Connect without touching the PC**: notice that Second Screen was opened
  without scanning Wi-Fi Direct all the time (its cost is above). Candidates:
  a change in the tablet's Samsung BLE advertisements (Bluetooth scanning does
  not disturb Wi-Fi), mDNS on the home network. Then opening the tile on the
  tablet would be the only step.
- **Open Second Screen without touching the tablet**: Windows wakes it over
  Bluetooth through Samsung MCF; reverse engineer the start intent
  (`com.samsung.android.mcfserver`, the Windows MCF library). This would remove
  the last tap.
- Persistent Wi-Fi Direct groups for faster reconnects.
- Mitigations for Intel `iwlwifi` firmware crashes during group formation.
- Android's built-in USB webcam mode (UVC, Android 14+, `ro.usb.uvc.enabled`):
  a zero-app camera for people who can plug the tablet in.
- Whether the companion app can start the display too (fewer apps involved
  when it is installed).
- Ink drawn on the tablet itself while signing, so it sits under the nib
  instead of a video frame behind (Notewise and Samsung Notes draw in the
  front buffer). It needs the pen's events next to Second Screen's window;
  plain overlay windows do not get them.

## Not planned

Clipboard sharing, phone notifications and remote input: GSConnect and KDE
Connect already do them well.
