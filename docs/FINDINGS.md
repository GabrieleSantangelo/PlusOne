# Samsung Second Screen: reverse-engineering notes

Device: Galaxy Tab S10 FE+ (SM-X620), One UI 8.5 / Android 16, Second Screen
`com.samsung.android.secondscreen` 1.0.17.9. Sink engine:
`/system/lib64/libremotedisplay_wfd.so`. Analysed with REA (JADX) for the APK
and `aarch64-linux-gnu-objdump` + `tools/re/arm64_strrefs.py` for the library.
Prior art for the Windows side: paulorochagodoi/segunda-tela-linux.

## Protocol

Plain Wi-Fi Display (Miracast) sink: the source runs the RTSP server on 7236,
sends M1, queries capabilities (M3), sets formats (M4), triggers SETUP (M5).
Media is H.264 in MPEG-TS over RTP (payload 33) to UDP 19000. The sink sends
RTCP receiver reports to the source's `server_port + 1`; if nothing listens
there the PC answers port-unreachable and the sink tears the session down.

## Source classification (sets sink latency)

`WifiDisplaySink::onOptionsRequest` / `onReceiveM2Response` look at the
source's messages:

| Text seen | Source type | latencyMode |
|---|---|---|
| `GStreamer RTSP server`, `AllShareCast/TizenTV` (M1) | 1, TV | 2 (high) |
| `Server: MSMiracastSource…` (reply to M2) | 2, Windows | 1 (mid) |
| `AllShareCast/Galaxy` (M1) | 3, Galaxy phone | 1 (mid) |
| anything else | 0 | 2 (default) |

`DirectRenderer::setSourceType` then applies `/sdcard/Documents/wfd/wfdconfig.txt`
(`WFDConfigParser`, lines of `key value`): `sink_latency_mode low` gives
latencyMode 0 (audio buffer 5 frames instead of 30). Other keys include
`width`, `height`, `fps`, `force_tcp`, `r2`, `hdcp`.

Only for type 2 does the sink answer `wfd_display_edid` (preferred timing
1920×1200@60, 30×20 cm).

## Resolution

The sink lays the picture out by the M4 mode, so the stream must match it.
`VideoFormats::mResolutionTable` (48 entries per table) includes VESA[29]
1920×1200@60 and VESA[33] 2560×1600@60 (the latter needs 48-bit R2
bitmaps / `wfd2_video_formats`). M4 with VESA bit 29 gives a full-screen 16:10
picture even though M3 does not advertise it.

## Latency (photo of PC preview vs tablet, 1920×1200@60 NVENC)

- gnome-network-displays (TV profile, 30 fps): clearly worst.
- Windows identity + `sink_latency_mode low`, with silent AAC: ~90 ms.
- Same, video-only MPEG-TS: ~45–50 ms. The sink accepts video-only streams.

`wfd_sec_low_latency_mode` and LLWFD are source-side (phone → TV) only. UIBC
input sets `UIBCDetected` and `DirectRenderer::setNonAVSyncMode(true)`.

## Starting the receiver

- `adb shell cmd statusbar click-tile com.samsung.android.secondscreen/.tile.ScreenSharingTile`
  opens it (the first click after a force-stop is sometimes ignored).
- Wi-Fi Direct: Second Screen approves an incoming GO negotiation late; the
  tablet completes it (GO-NEG-SUCCESS) but the PC often misses the
  confirmation (status=-1), likely because its single radio also serves the
  2.4 GHz AP while the group runs on 5745 MHz. Flaky on this setup.
- MS-MICE (cable mode): `InfrastructureServer` listens on 127.0.0.1:7250 when
  `sys.usb.config` contains `mtp` and the receiver saw `{"USB":1}` in
  `Android/data/com.samsung.android.secondscreen/files/config/secondscreen.data`
  (it deletes the file after reading). It also switches USB to `conn_gadget`.
  Over adb: `svc usb setFunctions mtp`, write the flag, `adb forward tcp:7250`,
  `adb reverse tcp:7236`, send SOURCE_READY = `len(2) 01 01` + TLVs
  (0: UTF-16LE friendly name, 2: RTSP port, 3: 16-byte source id). The sink
  dials back to 127.0.0.1:7236 within a second; RTP goes to the tablet's LAN
  IP. Each USB function change restarts adbd (new wireless-debugging port).

## Pen handling in note apps (for the signing window)

Notewise 4.1.4 (`com.yygg.note.app`) and Samsung Notes 4.4.45, pulled from
the tablet and read with REA (JADX, objdump); static analysis only.

- **Notewise smooths with Google's ink-stroke-modeler** (`ink::stroke_model`,
  Apache-2.0) in `libsmooth_motion.so`, behind `SmoothMotionJNI`
  (`smoothMotionDown/Move/Up(x, y, pressure, time)`, `predictMotionMove`,
  `resetInkStrokeModeler(handle, stylus, level)`). The native reset fills
  `StrokeModelParams` from the pen's *Stabilization* level L (0-10); times
  are passed in milliseconds and divided by 100, so in seconds:
  - spring-mass tip: mass/spring `(30 + 10 L²) / 32400 / 100` s², drag
    `(72 - 0.36 L²) * 10` /s, i.e. the ink trails the pen by `mass * drag`,
    from 7 ms (L=0) to 115 ms (L=10), always well damped;
  - wobble smoothing over 10 ms (stylus) or 50 ms, below 20-30 units/s;
    upsampling to at least 500 steps/s; end-of-stroke catch-up (0.01, 20
    iterations); loop-contraction mitigation off;
  - Kalman predictor: 24 ms ahead for the stylus (36 ms otherwise),
    `process_noise` 2.6e-4, `measurement_noise` 0.026, travel-speed
    confidence 25-400 units/s, acceleration 0.5, jerk 0.1.
  The Java side feeds every historical MotionEvent sample, skips samples
  identical to the previous one, spreads timestamps evenly when the batch's
  are all equal, drops modelled points closer than 0.2 units to the last
  kept one, and asks for the prediction only while moving.
- **Notewise widths** (`lsa.e`): fountain pen width =
  `size * ((k/2 + 1) * pressure + 1 / (2k + 2))` with `k = 6s / (s + 5)`, `s` the
  pressure sensitivity 0-10: linear in pressure above a floor. Strokes are
  filled outlines: each pair of samples becomes two circles (radius = half
  the width at each end) joined by their outer tangents, so the width changes
  smoothly inside a segment. Holding the pen still at the end of a line
  snaps it straight (`lsa.r`: RMS distance to the chord).
- **Samsung Notes** predicts with encrypted neural models in
  `libSPenPredictor.so` (M16/M20/M22: 16, 5.6 and 6.3 ms ahead), each fed
  through Kalman filters on position, pressure, tilt and orientation; not
  reusable.
- **What Plus One took** (`plusone/modeler.py`): the same model and L scale,
  the default L=2 (15 ms); our clock counts the pen's reports (285 a
  second) because they arrive over Wi-Fi in bursts; pressure-only reports
  (every other one) move time but not the velocity estimate. The prediction
  was retuned on a real signature: a straight line along the filtered
  velocity, 30 ms ahead at most, 0.12 px RMS off the line drawn next.
- The same data showed that most of Second Screen's stray samples hop less
  than 3 px but carry a pressure 0.06-0.15 off (the pen's own changes stay
  under 0.02 a report): `pen.Despike` drops those too.
