//! The virtual monitor (or a test pattern), encoded, to the app
//! (appdisplay.py AppStreamer). Each encoded frame is cut into packets,
//! sealed and sent on GStreamer's own streaming thread, the moment the
//! encoder has it, as few send calls as GSO allows; a thread of its own
//! listens to the app (fingers, pen, pings, its reports).

use super::input::Input;
use super::media::{cap_keyframes, Encoder, Keyframes, CAPTURE_FORMAT};
use super::rate::RateControl;
use crate::link::datagrams::{Datagrams, MTU_PAYLOAD, NONCE, TAG};
use crate::log;
use gst::prelude::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const VIDEO: u8 = 1;
pub const KEYFRAME: u8 = 2;
pub const PING: u8 = 3;
pub const PONG: u8 = 4;
pub const STATS: u8 = 5;
pub const INPUT: u8 = 6;
const VIDEO_HEADER: usize = 18;  // kind, flags, frame u32, index u16, count u16, captured i64
/// H.264 per packet: with the header, nonce and tag, one Ethernet frame.
pub const PAYLOAD: usize = MTU_PAYLOAD - VIDEO_HEADER - NONCE - TAG;
const STATS_SIZE: usize = 21;    // kind, shown u32, lost u32, 6 x u16 (tenths of ms)
const MIN_KEYFRAME_GAP: Duration = Duration::from_millis(250);  // the app asks again until one comes
const IDLE_WAIT: Duration = Duration::from_millis(250);  // the listener's sleep while nothing touches
const BUSY_WAIT: Duration = Duration::from_millis(20);   // ...and while something does (Input::check)
const QUEUE_MS: u32 = 60;  // waiting to go out past this many ms of the bitrate: the next frames not encoded
const TRUESIZE: f64 = 1.5;  // a packet waiting counts with its buffers (TIOCOUTQ): ~2.3 KB for 1472

/// The app's report of the last second, and what this side knows of it.
pub struct Stats {
    pub shown: u32,
    pub lost: u32,
    pub capture_to_decoded: [f64; 3],  // median, 90th percentile, most (ms)
    pub whole_to_decoded: [f64; 3],
    pub capture_to_sent: Option<f64>,
    pub skipped: u32,
    pub kbps: u32,
    pub changed: Option<(u32, u32)>,
}

pub fn monotonic_ns() -> i64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
    t.tv_sec * 1_000_000_000 + t.tv_nsec
}

struct Shared {
    channel: Arc<Datagrams>,
    enc: gst::Element,
    pipeline: gst::Pipeline,
    fps: u32,
    rate: Mutex<RateControl>,
    kbps: AtomicU32,
    skipped: AtomicU32,
    encoded: Mutex<Vec<f64>>,        // ms from capture to sent, since the last report
    frame: AtomicU32,
    running: AtomicBool,
    shown: AtomicBool,
    last_keyframe: Mutex<Option<Instant>>,
    keyframes: Mutex<Keyframes>,
}

impl Shared {
    fn force_keyframe(&self) {
        let mut last = self.last_keyframe.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < MIN_KEYFRAME_GAP) {
            return;
        }
        *last = Some(Instant::now());
        self.keyframes.lock().unwrap().request();
    }
}

pub struct Streamer {
    shared: Arc<Shared>,
    listener: Option<JoinHandle<()>>,
    in_probe: Option<gst::PadProbeId>,
}

pub struct Callbacks {
    pub on_stats: Box<dyn Fn(Stats) + Send>,
    /// From the listening thread: the app shows the desktop, or not.
    pub on_visible: Box<dyn Fn(bool) + Send>,
    pub on_error: Box<dyn Fn(String) + Send + Sync>,
}

impl Streamer {
    /// source: the virtual monitor's PipeWire node, or None for a test pattern.
    pub fn new(source: Option<u32>, channel: Arc<Datagrams>, (width, height): (u32, u32), fps: u32, bitrate: u32,
               encoder: &Encoder, input: Option<Input>, callbacks: Callbacks) -> Result<Streamer, glib::Error> {
        let video = match source {
            None => format!("videotestsrc is-live=true pattern=ball ! video/x-raw,format={CAPTURE_FORMAT},\
                            width={width},height={height},framerate={fps}/1 ! \
                            timeoverlay font-desc=\"Sans 90\" valignment=center halignment=center"),
            // Mutter gives the virtual monitor the stream's max-framerate as its refresh rate.
            Some(node) => format!("pipewiresrc path={node} do-timestamp=true always-copy=false keepalive-time=100 ! \
                                  video/x-raw,format={CAPTURE_FORMAT},width={width},height={height},max-framerate={fps}/1"),
        };
        let launch = format!("{video} ! queue max-size-buffers=1 leaky=downstream ! {} ! {} ! video/x-h264,profile=high ! \
                              h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au ! \
                              appsink name=out sync=false async=false max-buffers=0",
                             encoder.convert(), encoder.launch(bitrate, fps));
        let pipeline = gst::parse::launch(&launch)?.downcast::<gst::Pipeline>()
            .map_err(|_| glib::Error::new(gst::CoreError::Failed, "not a pipeline"))?;
        let enc = pipeline.by_name("enc").unwrap();
        cap_keyframes(&enc, bitrate, fps);
        // When each frame going in was captured, in order: one comes out per
        // one in (no B frames), and a flush drops those the encoder held.
        let captured_on_flush: Arc<Mutex<VecDeque<i64>>> = Arc::default();
        let flushed = captured_on_flush.clone();
        // The keyframes' probe first, as media.py: a flush, then this frame goes in.
        let keyframes = Keyframes::new(&enc, move || flushed.lock().unwrap().clear());
        let channel_app = channel.clone();
        let shared = Arc::new(Shared {
            channel, enc: enc.clone(), pipeline: pipeline.clone(), fps,
            rate: Mutex::new(RateControl::new(bitrate)), kbps: AtomicU32::new(bitrate), skipped: AtomicU32::new(0),
            encoded: Mutex::new(Vec::new()), frame: AtomicU32::new(0),
            running: AtomicBool::new(true), shown: AtomicBool::new(true), last_keyframe: Mutex::new(None),
            keyframes: Mutex::new(keyframes),
        });
        // The flush's own deque is the one frames are timed with.
        let in_probe = {
            let shared = shared.clone();
            let flushed = captured_on_flush.clone();
            enc.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                // The network behind: this frame is not encoded, rather than
                // queued after the others (the delay would grow, and grow).
                let kbps = shared.kbps.load(Ordering::Relaxed);
                let limit = ((kbps * QUEUE_MS / 8) as f64 * TRUESIZE) as usize;
                if shared.channel.queued() > limit.max(64 << 10) {
                    shared.skipped.fetch_add(1, Ordering::Relaxed);
                    return gst::PadProbeReturn::Drop;
                }
                // pipewiresrc and a live test source stamp buffers with the
                // running time at capture; the clock is the monotonic one.
                let captured = info.buffer().and_then(|buffer| buffer.pts()).and_then(|pts| {
                    let segment = pad.sticky_event::<gst::event::Segment>(0)?;
                    let segment = segment.segment().downcast_ref::<gst::ClockTime>()?;
                    let running = segment.to_running_time(pts)?;
                    Some((pad.parent_element()?.base_time()? + running).nseconds() as i64)
                }).unwrap_or_else(monotonic_ns);
                let mut deque = flushed.lock().unwrap();
                deque.push_back(captured);
                while deque.len() > 16 {  // never out of step for long
                    deque.pop_front();
                }
                gst::PadProbeReturn::Ok
            })
        };
        let sink = pipeline.by_name("out").unwrap().downcast::<gst_app::AppSink>().unwrap();
        {
            let shared = shared.clone();
            let captured = captured_on_flush.clone();
            sink.set_callbacks(gst_app::AppSinkCallbacks::builder().new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let at = captured.lock().unwrap().pop_front().unwrap_or_else(monotonic_ns);
                send_frame(&shared, map.as_slice(), !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT), at);
                Ok(gst::FlowSuccess::Ok)
            }).build());
        }
        {
            let on_error = callbacks.on_error;
            let bus = pipeline.bus().unwrap();
            bus.set_sync_handler(move |_, message| {
                if let gst::MessageView::Error(e) = message.view() {
                    log!("app display: pipeline error: {} {:?}", e.error(), e.debug());
                    on_error(format!("{}: {}", e.src().map(|s| s.name().to_string()).unwrap_or_default(), e.error()));
                }
                gst::BusSyncReply::Drop
            });
        }
        let address = channel_app.app().map_or("?".to_owned(), |a| a.to_string());
        log!("app display: {width}x{height}@{fps} {} {bitrate} kbit/s -> {address}", encoder.name);
        let listener = {
            let shared = shared.clone();
            let (on_stats, on_visible) = (callbacks.on_stats, callbacks.on_visible);
            Some(std::thread::Builder::new().name("plusone-display-in".into())
                .spawn(move || listen(shared, input, on_stats, on_visible)).unwrap())
        };
        Ok(Streamer { shared, listener, in_probe })
    }

    pub fn start(&self) -> Result<(), gst::StateChangeError> {
        self.shared.pipeline.set_state(gst::State::Playing).map(|_| ())
    }

    pub fn stop(&mut self) {
        self.shared.running.store(false, Ordering::Relaxed);
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
        let _ = self.shared.pipeline.set_state(gst::State::Null);
        self.shared.keyframes.lock().unwrap().remove();
        if let Some(probe) = self.in_probe.take() {
            if let Some(pad) = self.shared.enc.static_pad("sink") {
                pad.remove_probe(probe);
            }
        }
    }
}

/// Streaming thread: one frame, out at once.
fn send_frame(shared: &Shared, data: &[u8], keyframe: bool, captured: i64) {
    let count = data.len().div_ceil(PAYLOAD).max(1);
    let number = shared.frame.fetch_add(1, Ordering::Relaxed) + 1;
    let mut bodies: Vec<Vec<u8>> = Vec::with_capacity(count);
    for index in 0..count {
        let chunk = &data[(index * PAYLOAD).min(data.len())..((index + 1) * PAYLOAD).min(data.len())];
        let mut body = Vec::with_capacity(VIDEO_HEADER + chunk.len());
        body.push(VIDEO);
        body.push(keyframe as u8);
        body.extend_from_slice(&number.to_be_bytes());
        body.extend_from_slice(&(index as u16).to_be_bytes());
        body.extend_from_slice(&(count as u16).to_be_bytes());
        body.extend_from_slice(&captured.to_be_bytes());
        body.extend_from_slice(chunk);
        bodies.push(body);
    }
    let refs: Vec<&[u8]> = bodies.iter().map(Vec::as_slice).collect();
    if let Err(e) = shared.channel.send_all(&refs) {
        log!("app display: cannot send: {e}");
    }
    shared.encoded.lock().unwrap().push((monotonic_ns() - captured) as f64 / 1e6);
}

fn listen(shared: Arc<Shared>, mut input: Option<Input>, on_stats: Box<dyn Fn(Stats) + Send>,
          on_visible: Box<dyn Fn(bool) + Send>) {
    while shared.running.load(Ordering::Relaxed) {
        // Input.check() every 20 ms while something touches or waits; else seldom.
        let busy = input.as_ref().is_some_and(Input::busy);
        let body = shared.channel.receive(if busy { BUSY_WAIT } else { IDLE_WAIT });
        if let Some(input) = input.as_mut() {
            input.check();
        }
        let Some(body) = body else { continue };
        match body[0] {
            INPUT => {
                if let Some(input) = input.as_mut() {
                    if !input.packet(&body) {
                        log!("app display: a malformed input packet");
                    }
                }
            }
            KEYFRAME => {
                log!("app display: the app asks for a keyframe");
                shared.force_keyframe();
            }
            PING if body.len() == 9 || body.len() == 10 => {
                let mut pong = vec![PONG];
                pong.extend_from_slice(&body[1..9]);
                pong.extend_from_slice(&monotonic_ns().to_be_bytes());
                let _ = shared.channel.send(&pong);
                if body.len() == 10 {
                    let shown = body[9] != 0;
                    if shown != shared.shown.swap(shown, Ordering::Relaxed) {
                        if !shown {
                            if let Some(input) = input.as_mut() {
                                input.close();  // nothing left touching
                            }
                        }
                        // Nothing is captured or encoded while the app does not show it.
                        let _ = shared.pipeline.set_state(if shown { gst::State::Playing } else { gst::State::Paused });
                        if shown {
                            shared.force_keyframe();
                        }
                        on_visible(shown);
                    }
                }
            }
            STATS if body.len() == STATS_SIZE => {
                let shown = u32::from_be_bytes(body[1..5].try_into().unwrap());
                let lost = u32::from_be_bytes(body[5..9].try_into().unwrap());
                let ms: Vec<f64> = (0..6).map(|i| u16::from_be_bytes([body[9 + 2 * i], body[10 + 2 * i]]) as f64 / 10.0).collect();
                let mut encoded = std::mem::take(&mut *shared.encoded.lock().unwrap());
                encoded.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let skipped = shared.skipped.swap(0, Ordering::Relaxed);
                let mut rate = shared.rate.lock().unwrap();
                let before = rate.kbps;
                let changed = rate.report(shown, lost, ms[0], ms[3], skipped);
                if let Some(kbps) = changed {
                    shared.kbps.store(kbps, Ordering::Relaxed);
                    set_bitrate(&shared.enc, kbps);
                    cap_keyframes(&shared.enc, kbps, shared.fps);
                }
                on_stats(Stats {
                    shown, lost, capture_to_decoded: [ms[0], ms[1], ms[2]], whole_to_decoded: [ms[3], ms[4], ms[5]],
                    capture_to_sent: encoded.get(encoded.len() / 2).copied(), skipped, kbps: rate.kbps,
                    changed: changed.map(|k| (before, k)),
                });
            }
            _ => {}
        }
    }
    if let Some(input) = input.as_mut() {
        input.close();
    }
}

fn set_bitrate(enc: &gst::Element, kbps: u32) {
    if let Some(spec) = enc.find_property("bitrate") {
        let value = match spec.value_type() {
            t if t == glib::Type::I32 => (kbps as i32).to_value(),
            t if t == glib::Type::U64 => (kbps as u64).to_value(),
            _ => kbps.to_value(),
        };
        enc.set_property_from_value("bitrate", &value);
    }
}

/// What a report says, for the log.
pub fn describe(stats: &Stats) -> String {
    let d = stats.capture_to_decoded;
    let sent = stats.capture_to_sent.map_or("None".to_owned(), |s| format!("{s:.1}"));
    let mut text = format!("{} fps, lost {}; capture to decoded {:.1} ms (90%: {:.1}, max {:.1}); of which capture to \
                            sent {sent} ms, first packet to decoded {:.1} ms",
                           stats.shown, stats.lost, d[0], d[1], d[2], stats.whole_to_decoded[0]);
    if stats.skipped > 0 {
        text += &format!("; {} not encoded (the network behind)", stats.skipped);
    }
    text + &format!("; {} kbit/s", stats.kbps)
}
