# Changelog

## 0.4.0

Sign on the tablet.

- **Sign on Tablet**: right-click a PDF in Files (or *Open With*, or the
  *Sign on Tablet* launcher). Plus One connects the tablet if needed and opens
  the document full screen on it; the S Pen signs with pressure, fingers never
  draw (palm rejection) and pinch to zoom. *Done* saves `name-signed.pdf`
  next to the original, untouched, and the tablet disconnects again if it was
  connected for this.
- Hold the S Pen's side button to erase whole strokes; undo restores them,
  and also a cleared page.
- Pen colour (five inks) and thickness, down to a fine 0.3 pt (most of the
  slider is for thin pens), remembered; pressure shows in the line.
- A cleaner signing window: the page floats on a soft shadow (light and dark
  themes), tools sit in a floating bar under it with an eraser button, *My
  signature* shows your signature, a faint preview follows the pen while
  placing it, and messages are toasts.
- No more needles and bumps in pen strokes: GTK also reports the pen as a
  mouse drag (from a device that calls itself a mouse), and those late
  positions, with a fixed pressure, were mixed into the stroke.
- Ink like Notewise's: strokes go through a stroke model (the one Google's
  ink-stroke-modeler describes, which Notewise uses): the pen's jitter is
  smoothed, curves come out round and the stroke still ends exactly where
  the pen lifted. A *Stabilization* setting (0-10, as in Notewise) sets how
  much; while drawing, the line runs ahead to where the pen is predicted to
  be, which hides part of the delay.
- While the signing window fills the tablet it draws from the pen reports
  as they arrive (about 285 a second), not from desktop events.
- Saved signature (draw once, place with one tap), date and text fields, undo,
  page navigation.
- The signature is stamped onto the original pages (pypdf), so text, forms and
  metadata are kept; rotated pages are handled.
- Falls back to signing on the PC when the tablet does not connect, and to a
  save dialog when the folder is not writable (Flatpak).
- *Sign on Tablet* in Files shows only while a tablet is on the network
  (it says so through Quick Share or KDE Connect) or connected, and a switch
  in the window's options turns it off everywhere (Files, *Open With*, the
  app grid). The window shows which tablet it sees.
- D-Bus method `SignDocument(path)` and property `SignAvailable`.
- Connecting after a change of Wi-Fi network no longer takes minutes:
  - a docker or VPN interface going away no longer passes for the Wi-Fi
    device going away (which left Plus One waiting for it until it gave up);
  - Plus One keeps looking for the tablet for as long as it waits for it
    (NetworkManager stopped looking after 30 s, so a tablet opened later was
    not seen);
  - when the tablet drops out of sight between two attempts, Plus One looks
    for it again instead of failing on the old one until it gives up;
  - when the Intel Wi-Fi firmware crashes while joining the tablet, Plus One
    asks to close and reopen Second Screen (the tablet otherwise ignores new
    attempts for minutes) and waits the full time again; the request is a
    notification, so it shows with the window closed too.
- Asking again to sign a PDF that is waiting for the tablet, or already open,
  no longer opens it twice: *Done* closed one window, and the others kept the
  tablet connected.
- New dependencies: `gir1.2-poppler-0.18`, `python3-pypdf` (`.deb`); the
  Flatpak builds poppler 25.12 and pypdf.

## 0.3.0

Always ready, one click away.

- *Tablet* toggle in GNOME's Quick Settings (GNOME Shell 45–49): connect and
  disconnect with one click, state as subtitle, icon in the top bar while
  connected. Shipped in the `.deb` but off: turn it on from Plus One's window
  or the Extensions app (Plus One never enables shell extensions by itself).
  A zip for Flatpak users is attached to the release.
- Plus One runs in the background from login (*Ready in the background*, on by
  default): no window needed, nothing running on the radio while idle.
  D-Bus interface `io.github.gabrielesantangelo.PlusOne1` for the toggle,
  scripts and keyboard shortcuts; D-Bus activation starts the app on demand.
- Notifications: connected (with *Disconnect*), tablet not found, problems.
- Searching for the tablet is time-boxed (2 minutes after a click, 90 seconds
  after Second Screen closes) because Wi-Fi Direct scanning delays other Wi-Fi
  traffic (measured: 20% of packets over 30 ms while searching).
- Touch, resolution and quality settings apply from the next connection
  without restarting anything.
- Ending a session also closes its touch connection, which used to linger
  for about 30 s after a reconnect.
- New app ID `io.github.gabrielesantangelo.PlusOne` (needed for Flathub).
  Flatpak users: `flatpak uninstall io.github.santal.PlusOne` after
  installing this version.

## 0.2.0

- Flatpak bundle (GNOME 50 runtime) built in CI and attached to releases.
- Encoder `vah264enc` (GStreamer va plugin) for Intel/AMD where gstreamer-vaapi is gone.
- Touch kept working after Stop then Start in the same window (the input
  port stayed busy).
- Survives the Wi-Fi device being reset by the driver (seen with Intel
  firmware crashes) instead of hanging in "Connecting…".
- GNOME Network Displays detected over D-Bus, which also works in a sandbox.

Install: `sudo apt install ./plus-one_0.2.0_all.deb` (Ubuntu 24.04), or
`flatpak install --user ./plus-one_v0.2.0_x86_64.flatpak` (any distribution
with GNOME on Wayland; see the README for S Pen pressure in the Flatpak).

## 0.1.0

First release.

- Extends the GNOME desktop onto a Samsung Galaxy Tab through its built-in
  Second Screen app: no app to install on the tablet.
- Virtual monitor through Mutter (shows up in Settings → Displays). The
  resolution is read from the tablet's EDID (1920×1200 at 60 Hz on a
  Tab S10 FE+), or chosen by hand.
- Wi-Fi Direct link that reconnects by itself when Second Screen is reopened.
- Low latency (~50 ms measured): Windows source identity, video-only stream,
  NVENC/VA-API/x264 tuned for zero latency, tablet set to its low-latency mode.
- Finger touch, and the S Pen as a real Linux tablet with pressure and tilt.
- GTK4/libadwaita window in English and Italian.

Install: `sudo apt install ./plus-one_0.1.0_all.deb` (Ubuntu 24.04, GNOME on Wayland).
