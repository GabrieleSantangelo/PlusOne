# Changelog

## Unreleased (1.0)

- **The tablet's speakers no longer fall silent in the middle of a video**:
  Chrome makes a new stream after a pause or an ad and closes the old one
  a moment later, both with the same name. Plus One took the old one going
  for the new one too, and stopped the speakers while the video played on,
  until another program made a sound.
- **A steadier picture on a shared network**: the link judged how much
  goes through on the last fifth of a second, which one big frame of the
  desktop fills; it now takes half a second. With a download beside on
  an emulated home Wi-Fi, a 4 Mbit/s picture went from 65 % to 99 % of
  frames whole.
- **No more black minute after a burst of losses**: on a Wi-Fi whose
  queue overflowed, the link's QUIC read the lost packets as a path that
  carries smaller ones, and for a minute no packet of the desktop fitted:
  none went, the picture stood still. A local network carries 1500-byte
  packets; the link keeps them so, and where they do not pass it stays on
  TCP.
- **Low latency Wi-Fi**: Ubuntu keeps the Wi-Fi's power saving on, and
  the radio sleeping between beacons delayed every packet to and from the
  tablet by 15-30 ms. At the first start Plus One says so and offers to
  turn it off (your password, GNOME's dialog); Preferences → Low latency
  Wi-Fi turns it on again.
- **No sound round and round over Bluetooth**: with the PC and the tablet
  connected by Bluetooth, Android sent the tablet's sound to the PC, which
  played it back on the tablet's speakers. While the tablet is this PC's
  speakers, the PC disconnects the tablet's Bluetooth audio (A2DP), so it
  plays on its own speaker, and connects it again when they go (after a
  crash too).
- **The PC locked, the tablet touches nothing**: while this PC's session
  is locked (or another user's is in front), the tablet's fingers and pen
  reach nothing, whatever touched is let go at once, and the tablet's
  camera and microphone pause ("Paused while this PC is locked"); all
  goes on when it opens again. A tablet left about could tap at the lock
  screen before.
- **A new PC pairs only when you let it**: on the tablet, Plus One → Pair
  a new PC opens pairing for two minutes (one PC); otherwise a PC the
  tablet does not know learns nothing of it, and no pairing question
  comes up. The question hides other apps' windows over it, takes no tap
  through them, and says the PC's address under its name.
- **The camera and the microphone, PC by PC**: a yes on the tablet lets
  only the PC that asked; each PC's options on the tablet take it back. A
  yes given before for all PCs is asked again, PC by PC. Each time a PC
  turns one on, the tablet says so at the top of its screen, with Stop,
  and the PC says which program did.
- **The PC's screen, a signature and a pairing code stay out of
  screenshots**, screen recordings, casting and Recents on the tablet; a
  small pill on the desktop says it is the PC's screen.
- **The tablet is no longer known on the network**: its app announced its
  name, version and id to everyone; now a name of no one and a tag per
  paired PC, which only that PC recognises and which changes every
  quarter of an hour (its name only while you open pairing on it). An
  older Plus One on a PC finds the tablet where it last reached it, not
  by its announcement: update both.
- **One host on the network can no longer keep the PC out**: at most two
  handshakes at once from one address, and new ones at a rate, on the
  link and on the app's listener.
- **The desktop keeps its quality on a phone's hotspot**: a lost packet or
  a still screen no longer takes the picture down to a few Mbit/s; on the
  link's QUIC paths the picture follows the link's congestion controller,
  at once as it moves (plus-one-link tells the apps), not once a second.
- **The tablet's speakers on a weak network**: over the link, while its
  budget is under 8 Mbit/s, the PC's sound goes in Opus (160 kbit/s, its
  low-delay mode) instead of as it is (1.7 Mbit/s: half of a weak phone's
  hotspot), to an app that plays it; as it is again once the budget held
  over 12 Mbit/s for 10 s. The screen leaves the speakers their share.
- **Post-quantum key exchange**: the link's TLS uses the hybrid
  X25519MLKEM768 between this PC and the app (X25519 with a version from
  before).
- **Signing by cable**: a PDF to sign goes over the link too (plus-one-link),
  so the cable alone is enough; so does the tablet's app update.
- **A signed answer is bound to its PDF**: the tablet says the SHA-256 of
  the PDF it showed, and the PC keeps the marks only for that PDF.
- **Saved signatures shared only by choice**: they go between the PC and
  the tablet only if both choose it (the PC: Signing → Share them with the
  tablet; the tablet: the PC's options, under Paired PCs). Off at first:
  each side keeps its own. Nor do they move to a new tablet with the app.
- **The app's updates only from the PCs you allow**: when you pair a PC,
  the tablet asks whether to take the app's updates from it (yes at
  first); the PCs paired before keep updating it; each PC's options on
  the tablet change it. By cable alone the PC learns the app's version
  over the link, so it updates it there too.
- **The cable at full speed**: one way it went in bursts of 2 MB a few
  seconds apart (16 MB in a minute); now 16 MB take a second, both ways
  at about 18 MB/s each. What was queued while the cable was full waited
  for the next keepalive instead of going as soon as there was room.
  Both ways at once the cable carries about 37 MB/s, near what USB 2.0
  gives.
- **The desktop, the sound and the pen go ahead of a big transfer**: with
  a PDF or the app going over the cable, a realtime packet went and came
  back in 28 ms; now in 9 ms (3 ms with the cable idle). The link sends
  them ahead of what waits, hands the rest to TLS a little at a time, and
  keeps the system's own queues short (on Wi-Fi's TCP too).
- **A slow feature no longer breaks its channel, nor stalls the others**:
  each control channel has a window (1 MB) its sender waits on, and what
  arrives waits in a queue of its own until the feature takes it; before,
  a feature that did not read for a second had its channel closed, and
  the whole link waited that second.
- **On Wi-Fi, a big transfer gives way to the desktop and the sound**:
  the access point's queue (a phone's hotspot's) is not ours to reorder,
  so while realtime goes the link watches the delay its pings see and
  slows the transfer down when the network starts queueing (as LEDBAT
  does). With a transfer both ways at full speed, realtime went from
  41-57 ms to 12-14 ms (9 ms idle); the transfer then goes at about
  4 MB/s, and at full speed again as soon as nothing realtime goes.
- **A paused video no longer keeps the tablet's speakers on**: a stream
  paused (still open, linked) is no program playing; the speakers stop
  after their 3 s, and start again when it plays.
- **On Wi-Fi, the desktop and the sound marked as video** (DSCP AF41, WMM's
  video queue): ahead of bulk traffic on the air.
- **A big transfer never chokes the cable**: plus-one-link feeds a PDF or
  the app to the link only as fast as it goes, so the desktop and the sound
  sharing the cable go on meanwhile.
- **The desktop on a slow Wi-Fi**: on a home Wi-Fi at 2.4 GHz far from its
  router (about 1 Mbit/s left for it) the desktop stayed at its least,
  1500 kbit/s, above what the network took, and stuttered; it now goes down
  to 800 kbit/s. A keyframe the tablet asks for again is sent only once the
  last one could have come (a round trip, and its own time on the way):
  each one more went into the queue the first one waited in.

## 0.10.0

The link: the tablet by the Wi-Fi or the cable, moving between them
without losing the desktop; and the app only (Samsung's Second Screen
gone).

- **The link** (docs/LINK.md): the connection to the tablet is a layer of
  its own, in Rust, the same code on the PC (plus-one-link, started by
  its socket when Plus One first needs it) and in the app (through JNI).
  The desktop, the speakers, the camera and the microphone go over it; the
  link of before stays beside it for signing and updates, and for a PC or
  an app without it.
- **The tablet by cable**: plugged in, it goes by USB (Android Open
  Accessory), the fastest way: round trips of about 4 ms instead of 9-11
  over a good Wi-Fi, nothing lost, the picture's bitrate kept. Android
  asks once whether Plus One opens the accessory. "Use the cable" in the
  preferences turns it off (the tablet's file transfer is off while Plus
  One has the cable).
- **Moving without ending**: the cable pulled, the session goes on over
  the Wi-Fi about 1.6 s later; plugged in again, back on the cable; both
  devices moved to another network (the home Wi-Fi to a phone's hotspot),
  the desktop comes back on it by itself, within a minute. What the PC
  said meanwhile arrives, once and in order.
- **What it is connected by**, everywhere: in GNOME's Quick Settings and
  in the window ("Connected · cable"); on the tablet, a small pill over
  the desktop and, at each change, a screen that says what was lost and
  what it is on now, until the picture comes again; on the app's main
  screen, "Connected to <PC> · via cable".
- **The tablet found without multicast too**: where it was last reached,
  then mDNS, which plus-one-link reads itself.
- **Chrome's sound no longer stops after about 10 s** on the tablet's
  speakers: Plus One no longer forces a 5 ms cycle on all the PC's sound
  (Chrome, writing in larger pieces, paused itself at that; Firefox did
  not), which also wakes the CPU four times less often.
- **The speakers bridge the Wi-Fi's stalls**: a stall's packets coming
  at once no longer grow the tablet's wait for good, and a hole is
  covered (up to 200 ms) instead of heard; no click between sounds.
- **The desktop's bitrate drops sooner** when frames are not encoded for
  what waits to go out, and climbs back carefully under where a queue
  showed.
- **Samsung's Second Screen is gone**: the desktop goes only through Plus
  One's app, which does all it did and better (about 22 ms instead of
  45-50, the picture following the network, the fingers as a touchpad).
  With it go Wi-Fi Direct, RTSP, MPEG-TS, UIBC, the adb option, the
  "Second screen through" choice, signing full screen on the tablet's
  monitor through it, and NetworkManager as a dependency (the Flatpak no
  longer builds libnm). When the app cannot show the desktop, Plus One
  says why (not on the network, not answering, not allowed to come to the
  front, another version, the pairing forgotten) instead of trying Second
  Screen; when it cannot sign, the notification offers signing on this
  PC. Settings of Second Screen in settings.json are ignored.
- The command line runs the desktop in the app (`--size` one of the
  tablet's sizes, `--test` a test pattern).
- The package is now amd64 (plus-one-link is a compiled program). The
  Flatpak keeps the link of before for now.

- **Samsung's Second Screen is gone**: the desktop goes only through Plus
  One's app, which does all it did and better (about 22 ms instead of
  45-50, the picture following the network, the fingers as a touchpad).
  With it go Wi-Fi Direct, RTSP, MPEG-TS, UIBC, the adb option, the
  "Second screen through" choice, signing full screen on the tablet's
  monitor through it, and NetworkManager as a dependency (the Flatpak no
  longer builds libnm). When the app cannot show the desktop, Plus One
  says why (not on the network, not answering, not allowed to come to the
  front, another version, the pairing forgotten) instead of trying Second
  Screen; when it cannot sign, the notification offers signing on this
  PC. Settings of Second Screen in settings.json are ignored.
- The command line runs the desktop in the app (`--size` one of the
  tablet's sizes, `--test` a test pattern).

## 0.8.1

Updates by themselves; the fingers as on a touchpad; the picture that
follows the network; the speakers in time; and the fixes of a review of
the whole code, bugs and security. (0.8.0 was never published.)

- **The speakers in time with the picture**: the Wi-Fi stalling, then
  handing over what it held at once, grew the tablet's wait for good
  (119 to 372 ms seen); now a nearly silent packet is left out while it
  is too long, and far over it the rate goes up to 1% for seconds: 27-32 ms
  waiting, the speaker about 135 ms behind the PC all told (what Android's
  output holds included), in time with the lips. And a short gap no longer
  throws away what the player still held (the sound that jumped): it waits,
  and only after 0.3 s of nothing does it rest.

- **The picture follows the network**: through the app, the bitrate set
  is the most; it goes down by a third as soon as the tablet reports
  frames lost or the delay through the network growing, and up by a tenth
  after a few clean seconds. A frame is not even encoded while the data
  before it is still waiting to go out, so the delay never piles up. On a
  2.4 GHz network with a weak signal (2-5 Mbit/s for the tablet), the
  delay went from 100-170 ms, with seconds of frozen picture, to about
  22 ms.
- **Fingers as on a touchpad**, where they touch: one tapped clicks
  (Qt's programs, Telegram, took a touch tapped for hover and wanted a
  second tap), moved drags, held is the right button; two tapped are the
  right button, moved together scroll, smoothly, and a flick goes on and
  slows down as on a phone until a finger touches; two moved apart pinch
  (browsers zoom the picture, as with a touchpad); three swiped show the
  overview, leave it, or change workspace.
- **Updates by themselves**: each release is also an APT repository,
  signed; the `.deb` installs its source and key, so `apt upgrade` and
  Software Updater find new versions and unattended-upgrades installs
  them. Plus One sees it was replaced and starts again once nothing is in
  use, saying so. The tablet's app is brought to the PC's version by the
  PC: the app of its release goes over the paired link while nothing is
  in use, and the app installs it, without a question after the first
  time (Android 12 and later). From 0.8.1 on: the first time, install both
  by hand.
- Fixed: the webcam showed no black while the tablet was away or
  refusing (and froze on the last picture when its session dropped): its
  black frames' timer and the in-use one were the same.
- Fixed: a tablet gone from the network (off, out of range) left the
  desktop in the app "connected" for about 15 minutes, and the speakers
  sending into nothing: a session's link now gives up after 20 s without
  the tablet acknowledging.
- Fixed: an encoder error, or the screen sharing stopped from GNOME's
  indicator, left the tablet on a frozen desktop; the session ends and
  says why (the tablet gone, or the error).
- Fixed: a large keyframe (HiDPI sizes) could lose its tail when the send
  buffer filled; the tablet asked for it again and again.
- Fixed: the tablet's screen going off and on popped "Tablet connected"
  each time.
- Fixed: with the speakers on (and the background off), closing the
  window quit Plus One, and the speakers went with it.
- Fixed: one mDNS answer lost took the speakers away for a moment.
- Fixed: Ctrl+Q while signing lost the marks without asking.
- Fixed: pairing another tablet while connected switched the tablet in
  use mid-session.
- Fixed: the window's "Ready in the background" did not follow the Quick
  Settings toggle; the tablet's sizes appeared only after reopening it;
  a missing adb cleared the "Open Second Screen from here" choice; the
  speakers' "needs" text was the microphone's; a program name with "&"
  did not show.
- Fixed: turning off "Sign on Tablet" (Files' entry) also stopped signing
  in the tablet's app.
- Fixed: picking the pen in the signing window left Select on.
- Signature thumbnails on white, readable in the dark style.
- Less while idle: digital silence (a paused player keeping its stream)
  is not sent to the speakers after a second; the desktop's listener
  wakes 4 times a second instead of 50 while nothing touches; the app
  pings every 10 s while the desktop is hidden, and the stats are logged
  every 30 s.
- The app: a newer desktop session ends the one before (its PC told);
  a closing desktop screen no longer pauses or ends the next session;
  the decoder is made again after an error, and no keyframes are asked
  for while there is none; the desktop is letterboxed when Android 16
  turns a large screen's app to portrait; camera and microphone allowed
  while the desktop is in front without "display over other apps".
- Security hardening in the app: connections from the tablet itself
  (another app posing as a PC) are closed; Pair works a moment after the
  question is in front, not under a tap meant for something else; the
  PC's name is shown without control or direction-changing characters.

## 0.7.0

The desktop in Plus One's app, and the tablet's speakers.

- **The second screen through Plus One's app**: with the app on the
  tablet and on the same Wi-Fi, *Use as second screen* (or the Tablet
  button) sends the desktop to it, no Second Screen and no Wi-Fi Direct.
  H.264 frames go out the moment the encoder has them, cut into UDP
  packets sealed with AES-GCM under a key the paired link hands over; the
  app decodes each as soon as it is whole (hardware, low-latency mode) and
  shows it at once. Measured at 1920×1200@60: 23 ms median from capture on
  the PC to decoded on the tablet while idle, about 30 while windows move
  (Second Screen: 45-50 ms to the glass). Fingers and the S Pen go back the
  same way, every pen sample, pressure, tilt, hover and the side button;
  two fingers tapped together are the right button. Paused while the app is
  not on the tablet's screen; Back or *Stop* on the tablet ends it. Without
  the app, or by choice in the preferences, Second Screen as before. New
  dependency: `python3-cryptography`.
- **The desktop at the tablet's own size**: through the app, Resolution
  offers the tablet's sizes at its proportions (2880×1800, 2560×1600,
  2304×1440, 1920×1200...); from 2560 wide the virtual monitor is put at
  scale 2, sharp text with the room of half the size. The bitrate grows
  with the pixels. Automatic stays at 1920×1200: larger, the tablet's
  decoder takes longer (about 45 ms instead of 25 at 2880×1800).
- **The tablet's speakers**: a sound output, *Tablet speakers*, while the
  tablet's app is on the network. Its sound goes as it is, 16-bit stereo
  PCM at 48 kHz in encrypted 5 ms packets, and the app plays it as the
  tablet's own players do (a first try through libwebrtc, made for calls,
  squeezed the music); the two clocks' drift is followed by trimming the
  playback rate. Only while a program plays; the switch makes it the output.
- **A new window**: a sidebar of what Plus One does (Second screen,
  Signing, Camera, Microphone, Speakers, Tablets, General), each page with
  its button and every setting in view; no menu, no Advanced, no
  preferences dialog. A dot marks what is on, green while it works.
- **The tablet found sooner**: an ask that the tablet (asleep) did not
  answer is tried again within seconds, not a minute later; and where its
  app was last is tried when it is not announced.
- Fixed: after a loss, Second Screen could stay broken up to 10 s with
  Intel's encoder, which answered keyframe requests with a frame the
  tablet cannot start from, and only once; each request now gets a real
  keyframe at the next frame.

## 0.6.0

Your tablet, and only it.

- **Pairing from the window**: *Pair a tablet* lists the tablets with
  Plus One's app on your Wi-Fi; pick one, compare the six-digit code on
  both screens, confirm on both. Signing no longer pairs by itself, and the
  app only pairs when the PC's user asked for it.
- **One tablet in use**, for the desktop and for the PDFs: several can be
  paired, the one in use is picked under its name in the window; *Paired
  tablets* in the menu forgets one. Other tablets on the network get
  nothing (Files no longer offers a choice of tablet).
- **Second Screen checked with the app**: once a tablet joins over Wi-Fi
  Direct, and before any picture leaves the PC, Plus One connects to
  Plus One's app over that same link and wants the paired certificate, a
  tablet that knows this PC, and the address it came from (so nobody can
  pass the check on to the real tablet). Otherwise the desktop is not sent
  and the window says why. This replaces 0.5's "the first tablet streamed
  to is the only one", which trusted a name anyone can copy. Plus One's app
  must run on the tablet for Second Screen too.
- **A new window**: the tablet in use, its state and two buttons (*Use as
  second screen*, *Sign a PDF…*); everything else in the menu: preferences,
  My signatures, Paired tablets, Log, About. It says what is happening and
  what to do next, with the button that helps (an error offers *Try
  again*, *Open preferences*, *Show log*), and who uses the camera and
  microphone ("In use by Firefox"). Ctrl+, Ctrl+W, Ctrl+Q.
- **Preferences on one page**, in words: the picture *Balanced*, *Sharper
  text* or *For weak Wi-Fi* instead of Mbit/s; resolution, touch and adb
  under *Advanced*. Plus One speaks the system's language (no setting).
  After the first pairing it offers the *Tablet* Quick Settings button
  once; it is turned on only if *Add* is pressed.
- **The tablet's camera and microphone for the PC**: a webcam (*Plus One
  tablet camera*, a v4l2loopback device that every program sees) and a
  microphone (*Tablet microphone*, a PipeWire source), each with its own
  switch in the window. The tablet captures only while a program on the
  PC uses them, so nothing goes over the network otherwise, and the switch
  off removes the device. Over WebRTC: libwebrtc on the tablet (gain
  control, optional noise suppression, congestion control, retransmissions,
  H.264 and Opus), GStreamer's webrtcbin on the PC (jitter buffer), the
  offer and answer over the paired TLS link, which vouches for the DTLS
  keys; about 160 ms from the tablet's microphone to a program on the PC.
  Front or back camera, 720p or 1080p. The app lets the PC have them only
  once the user turns them on there (*For the PC*), and says so in its
  notification while the PC uses them. When a program on the PC wants one
  that is off, the tablet asks ("santal-XPS wants to use the camera",
  *Allow*, with the tablet unlocked) and the PC says to look at it. The microphone's noise reduction,
  in the preferences: neural on the PC (DeepFilterNet, its plugin in
  `~/.ladspa`), light on the tablet (libwebrtc's) or off; then a compressor
  and a limiter bring the voice to about -18 dBFS. New dependencies:
  `gstreamer1.0-nice`, `gir1.2-gst-plugins-bad-1.0`, `swh-plugins`.
- The app listens on a fixed port (47236), so the PC finds it over Wi-Fi
  Direct at once. Protocol version 3: update the app and Plus One together.
- **The S Pen without a group**: a small helper, started by D-Bus when the
  pen is first used and allowed by polkit to the user at the screen, makes
  the virtual tablet and takes only pen reports for it; nobody gets
  `/dev/uhid` (with it a program could make a keyboard). 0.5's `plus-one`
  group is no longer needed (the package says how to remove it). New
  dependencies: `dbus`, `polkitd`.
- **Second Screen on Intel's low-power encoder** where there is one (VA-API
  `vah264lpenc`), before NVIDIA's: the discrete GPU stays asleep, about 8 W
  less, at 60 fps.
- **The app sets itself up**: its first screen asks for notifications and
  "display over other apps" in one card that says what for; Second
  Screen's low-latency file (`Documents/wfd/wfdconfig.txt`) is made by the
  app, not by hand. After an update the app is ready again by itself.
- **Second Screen kept to the tablet**: connections from the home network
  are refused at once, touches and RTCP are taken only from the Wi-Fi
  Direct group, the pen moves only over the tablet's monitor (else it is a
  mouse), a tablet is picked by its address, Disconnect means no session
  after it.
- **Signing never loses what was signed**: the copy sent is kept until the
  signed one is saved (next to the original, else in Documents); a PDF
  that cannot be signed (not a PDF, too big, locked) is said at once.
  Pairing questions are limited per address; forgetting a PC on the tablet
  ends its links.
- **Lighter**: the webcam costs nothing while no program shows it (it was
  10% of a core), a frame sooner; the microphone's neural noise reduction
  works only on speech; the signing window renders a page's tiles in one
  pass (a scanned page: 150 ms instead of 0.8 s), the app's signing view
  draws once a frame. No UPnP for the camera's and microphone's WebRTC
  (libnice asked the router to open the port to the internet).
- Releases: the app is built without the key and signed in a job of its
  own.

## 0.5.0

The companion app: ink under the nib.

- **Plus One for the tablet** (`android/`, Kotlin): Sign on Tablet sends the
  PDF to it over the home network, and the ink is drawn on the tablet
  itself, from every pen sample, into the front buffer (like a note app),
  instead of reaching it as video through Second Screen. No Wi-Fi Direct.
- The PDF is rendered by the tablet, sharp at any zoom; the same tools as on
  the PC (pen colour and thickness, the saved signature, date, text, the
  S Pen's side button to erase, undo, clear, pages; pinch and double-tap to
  zoom, fingers ignored while the pen is near). Pen and saved signature are
  shared with the PC.
- Paired once by comparing a six-digit code on both screens; TLS with a key
  kept by Android's keystore on the tablet. Nothing else on the network can
  send PDFs to the app or read what is signed.
- Opens by itself (with the "display over other apps" permission) or from a
  notification; always ready in the background by default, with a quiet
  notification, or only for a while after the app is opened.
- A Wi-Fi drop while signing does not lose the signature: Plus One finds the
  app again and gets it when the user is done.
- **Several signatures**, on the PC and in the app: "My signature" places
  the last used with one tap, its arrow lists them (pick, remove, draw a new
  one on a pad). Kept in step between PC and tablet at each signing: what
  either side added is kept, what either side removed stays removed. 0.4's
  single saved signature becomes the first.
- **Select, move, turn and scale** what is on the page, on the tablet and
  on the PC, as note apps do: a lasso picks what it touches, a tap picks
  one mark; a box with a handle at each corner (scale) and a turn handle
  (the angle shown, snapping to right angles); drag from inside; Delete.
  A placed signature, date or text comes picked, to adjust it at once.
- The eraser works in the new-signature pad too.
- **A queue of PDFs**: select five PDFs in Files and Sign on Tablet, or send
  them one by one, also while signing: they reach the tablet in the order
  sent (one pairing question for all of them) and wait there. Done or ✕
  sends one back and opens the next; the documents button (and the app's
  main screen, and a notification) lists them, to sign them in any order,
  keeping what was drawn on each. The PC tells about them in one
  notification ("5 PDFs to sign on…", "Signed 5 PDFs"); its file chooser
  takes several PDFs. A PDF whose PC stopped waiting leaves the queue.
- **Scrolling through the pages**, as a note app shows a notebook: a PDF's
  pages one under the other, a finger scrolls (a flick carries on), the
  page in the middle of the screen counted at the top; the pen draws on
  whichever page it touches. An option in the app (*Documents of several
  pages*) brings back one page at a time with the arrows.
- **Pages there at once while scrolling**: each page is rendered whole at
  a modest resolution as soon as it comes into view (and the ones next to
  it ahead), then sharp tiles at the zoom, also while it still moves; both
  kept in memory, so going back shows them at once.
- **More than one tablet**: Sign on Tablet in Files becomes a menu of the
  tablets with the app on the network; the window's *Tablet for signing*
  picks one for good, or *Automatic*: the last one used (before, the one
  with the lowest address). A dropped link is picked up from the same
  tablet, never another.
- **Material 3 Expressive** look for the app, in the wallpaper's colours,
  light or dark as the system: grouped settings as in Android 16; signing
  under floating bars only (close and title, pages and queue, Done, the
  tools), no bar across the screen; Material dialogs, sliders and
  switches; a new icon.
- The app fits phones too (Android 13 or later): icons in the toolbar,
  clear of the system bars and cutouts, popups sized to the screen.
- When the app is not there, Second Screen as before.
- Sturdy: a Wi-Fi drop while signing is noticed within a minute and the
  signature still arrives; turning the tablet, dark mode, a keyboard cover
  or split screen keep the ink; Back asks before throwing a signature away;
  several PDFs at once share one pairing question.
- **Paired tablets** in the window, each with *Forget*: signing on it again
  asks for the code.
- **The S Pen's device for a group you join**: `/dev/uhid` is for the
  members of the `plus-one` group, no longer for whoever is logged in (any
  program they run could create a keyboard as well). Join it once with
  `sudo usermod -aG plus-one $USER` and log in again; until then the
  window says so and the pen works as a mouse.
- **Second Screen only for the tablet**: the stream, the touches and the
  pen are for the tablet in the Wi-Fi Direct group, no longer for anyone on
  the network while Plus One waits; the pen's device is always the S Pen's,
  whatever the other side describes (it could have been a keyboard). After
  the first session only that tablet gets the desktop (*Forget* in the
  window for another).
- The app and the PC check each other harder: an app announced as a paired
  tablet with another certificate is refused, an unpaired one gets small
  limits and little time, a busy or stuck PC does not hold back the others,
  and what comes back from the tablet (marks, signatures) is validated.
- Settings, signatures and pairings are files only the user can read.
- Signed PDFs: the ink is added on top of the page, which is left as it was
  (with Ubuntu's pypdf some PDFs got it in the wrong place, or grew); crop
  boxes, any script in text; a PDF with a digital signature is signed, and
  the notification says that signature no longer holds in the copy.
- Fixed: undo with the pick on another page lost it; closing the signing
  window threw away the marks without asking; a resize handle jumped; narrow
  signatures came out too tall; the pad drew with the palm.
- Builds in a container (`tools/android-docker.sh`, `Dockerfile.android`),
  nothing Android on the host; CI builds it, releases attach the signed APK.
- New dependency: `openssl` (the PC's certificate).

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
- Ink like a note app's: strokes go through a stroke model (the one
  Google's ink-stroke-modeler describes): the pen's jitter is
  smoothed, curves come out round and the stroke still ends exactly where
  the pen lifted. A *Stabilization* setting (0-10) sets how
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
- Less lag:
  - each frame reaches the tablet a frame sooner: the MPEG-TS muxer and RTP
    payloader held every frame's last packets until the next frame came
    (16 ms at 60 Hz, up to 100 ms for the last change on a still screen);
    Plus One now sends each frame whole as soon as it is encoded;
  - the signing window draws 60 frames a second while you write (it managed
    33-40): the page is a texture made once, and a frame only draws the
    newest bit of the stroke (3.5 ms a frame instead of 10);
  - the eraser keeps up: it skips marks nowhere near it (checking every
    segment of every mark took 7 ms per pen sample on a short signature),
    and erasing or undoing redraws only that area of the page, whose
    texture is cut in tiles so only the tiles it covers are uploaded again;
  - zooming in and out stays smooth: while fingers pinch the page is only
    magnified (a frame took up to 144 ms rendering it again), only the
    tiles on screen are rendered, and tiles not ready yet render a few per
    frame over a small copy of the whole page instead of in one go (up to
    200 ms after a pinch, 44 ms frames while zooming out);
  - the first connection after Plus One starts is 1.5 s quicker: the NVENC
    plugin (CUDA probing the GPUs) loads while the tablet is being found,
    not after it connected;
  - far fewer lost packets: on a still screen, 1 in two minutes instead of
    35 in a side-by-side test, and each one made the tablet ask for a whole
    new frame (black flashes, ghost cursors).
- No more getting stuck:
  - no Wi-Fi Direct device, NetworkManager unreachable, the virtual monitor
    or the pipeline failing, or adb timing out now end with an error instead
    of waiting (or showing "Connected") forever;
  - searching for the tablet gives up on time even when it stays listed
    but never answers (it could keep the radio busy for an hour);
  - the tablet resuming a paused session no longer starts a second
    virtual monitor and stream;
  - fingers held down are released whatever ends the touch connection;
  - each session's pipeline is freed when it ends;
  - the Wi-Fi Direct group no longer comes up after *Disconnect* when the
    two crossed.
- *Settings* opens the settings window even while a signing window is open;
  *Cancel* and closing wait while the signed copy is being saved.
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
