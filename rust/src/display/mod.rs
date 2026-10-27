//! The desktop in Plus One's app on the tablet (appdisplay.py AppDisplay):
//! a session with the paired tablet's app, its virtual monitor (or a test
//! pattern), the stream, the fingers and the pen, for as long as the app
//! shows it.

pub mod input;
pub mod media;
pub mod monitor;
pub mod pen;
pub mod rate;
pub mod streamer;

use crate::link::{self, Datagrams, Paired, Target};
use crate::{log, msg};
use input::Input;
use monitor::{Injector, VirtualMonitor};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use streamer::{Callbacks, Streamer};

/// Widths offered for the desktop in the app, each at the tablet's
/// proportions where that comes out in whole, even pixels.
const WIDTHS: [u32; 7] = [3840, 2560, 2304, 1920, 1680, 1440, 1280];
const REFERENCE: u32 = 1920 * 1200;  // the size the bitrate presets are for
const HIDPI_WIDTH: u32 = 2560;       // from this wide, the monitor at scale 2
const STATS_EVERY: u32 = 30;         // the app's reports (one a second) logged: one in so many, and each with a loss

/// The sizes the desktop can have on a tablet of this screen, largest first.
pub fn sizes(screen: (u32, u32)) -> Vec<(u32, u32)> {
    let (width, height) = (screen.0.max(screen.1), screen.0.min(screen.1));
    let ratio = width as f64 / height as f64;
    let mut found = vec![(width, height)];
    for w in WIDTHS {
        let h = ((w as f64 * height as f64 / width as f64 / 2.0).round() * 2.0) as u32;
        if w < width && (w as f64 / h as f64 - ratio).abs() < 0.005 * ratio {
            found.push((w, h));
        }
    }
    found
}

/// The size closest to 1920x1200 in pixels, at the tablet's proportions.
pub fn auto_size(screen: (u32, u32)) -> (u32, u32) {
    sizes(screen).into_iter().min_by_key(|&(w, h)| (w as i64 * h as i64 - REFERENCE as i64).abs()).unwrap()
}

/// The presets' bitrate (for 1920x1200) for this many pixels.
pub fn bitrate_for(bitrate: u32, (w, h): (u32, u32)) -> u32 {
    (bitrate as f64 * (w as f64 * h as f64 / REFERENCE as f64).max(1.0)) as u32
}

pub struct Options {
    pub size: Option<(u32, u32)>,
    pub fps: u32,
    pub bitrate: u32,
    pub encoder: String,
    pub touch: bool,
    pub test: bool,
    /// With the test pattern: the fingers and the pen taken as on the
    /// desktop, given to nothing (bench.rs measures what they cost).
    pub bench_input: bool,
}

#[derive(Default)]
struct State {
    channel: Option<Arc<Datagrams>>,
    monitor: Option<Arc<VirtualMonitor>>,
    streamer: Option<Streamer>,
    mode: (u32, u32, u32),
    started: bool,
}

struct Inner {
    stopped: AtomicBool,
    state: Mutex<State>,
    options: Options,
    on_ended: Box<dyn Fn(Option<String>, bool) + Send + Sync>,
    reports: AtomicU32,
}

/// A display session; its callbacks run on the main loop.
#[derive(Clone)]
pub struct Session(Arc<Inner>);

impl Session {
    pub fn start(targets: Vec<Target>, app_id: String, options: Options,
                 on_ended: impl Fn(Option<String>, bool) + Send + Sync + 'static) -> Session {
        let session = Session(Arc::new(Inner {
            stopped: AtomicBool::new(false), state: Mutex::default(), options, on_ended: Box::new(on_ended),
            reports: AtomicU32::new(0),
        }));
        let me = session.clone();
        std::thread::Builder::new().name("plusone-display-link".into())
            .spawn(move || me.run(targets, app_id)).unwrap();
        session
    }

    /// On the main loop: everything ends now; the app is told by the link's
    /// thread (it sends "stop" and closes).
    pub fn stop(&self) {
        if !self.0.stopped.swap(true, Ordering::SeqCst) {
            self.teardown();
        }
    }

    fn stopped(&self) -> bool {
        self.0.stopped.load(Ordering::SeqCst)
    }

    // -- the link's thread -----------------------------------------------------------
    fn run(&self, targets: Vec<Target>, app_id: String) {
        let result = (|| -> Result<(), String> {
            let mut link = link::connect(&targets, Some(&app_id), &Paired::default(), false).map_err(|e| e.to_string())?;
            if self.stopped() {
                return Ok(());
            }
            let channel = Arc::new(Datagrams::new(None).map_err(|e| e.to_string())?);
            self.0.state.lock().unwrap().channel = Some(channel.clone());
            let reply = link.request(&msg!({"type": "display", "key": link::trust::hex(&channel.key),
                                            "port": channel.port()}), None, link::wire::MAX_HEADER)
                .map_err(|e| e.to_string())?;
            if reply.get("type").and_then(Value::as_str) != Some("display-ready") {
                return Err(format!("the app answered {:?}", reply.get("type")));
            }
            let number = |k: &str| reply.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
            let address = link.peer().map_err(|e| e.to_string())?;
            channel.set_app((address, number("port") as u16).into());
            let screen = (number("width"), number("height"));
            log!("app display: {address} shows {}x{} at {:.0} Hz", screen.0, screen.1,
                 reply.get("refresh").and_then(Value::as_f64).unwrap_or(0.0));
            let options = &self.0.options;
            let (width, height) = match options.size {
                Some(size) if sizes(screen).contains(&size) => size,
                _ => auto_size(screen),
            };
            self.0.state.lock().unwrap().mode = (width, height, options.fps);
            let me = self.clone();
            glib::MainContext::default().invoke(move || me.open());
            // Until the app hangs up (Back on the tablet), is gone, or stop().
            link.hold(|| self.stopped(), || Ok(()), Duration::from_millis(500)).map_err(|e| e.to_string())?;
            let _ = link.send(&msg!({"type": "stop"}), None);
            Ok(())
        })();
        if !self.stopped() {
            let me = self.clone();
            let error = result.err().or_else(|| Some("the tablet ended it".into()));
            glib::MainContext::default().invoke(move || me.ended(error));
        }
    }

    // -- the main loop ------------------------------------------------------------------
    fn open(&self) {
        if self.stopped() {
            return;
        }
        if self.0.options.test {
            return self.stream(None);
        }
        let (ready, closed) = (self.clone(), self.clone());
        match VirtualMonitor::start(move |node| ready.stream(Some(node)),
                                    move || closed.ended(Some("the screen sharing was stopped".into()))) {
            Ok(monitor) => self.0.state.lock().unwrap().monitor = Some(Arc::new(monitor)),
            Err(e) => self.ended(Some(e.message().to_owned())),
        }
    }

    fn stream(&self, source: Option<u32>) {
        let mut state = self.0.state.lock().unwrap();
        if self.stopped() || state.streamer.is_some() {
            return;
        }
        let (width, height, fps) = state.mode;
        let options = &self.0.options;
        let Some(encoder) = media::pick_encoder(&options.encoder) else {
            drop(state);
            return self.ended(Some("no H.264 encoder available".into()));
        };
        let input = if options.bench_input && state.monitor.is_none() {
            let started = Instant::now();
            Some(Input::new(Arc::new(crate::bench::Nothing), (width, height),
                            Box::new(|| Some(Box::new(crate::bench::Nothing) as Box<dyn pen::Pen>)),
                            Box::new(move || started.elapsed().as_secs_f64())))
        } else { match (&state.monitor, options.touch) {
            (Some(monitor), true) => {
                let injector: Arc<dyn Injector> = monitor.clone();
                let for_pen = injector.clone();
                let started = Instant::now();
                Some(Input::new(injector, (width, height),
                                Box::new(move || Some(pen::for_monitor(for_pen.clone(), (width as f64, height as f64)))),
                                Box::new(move || started.elapsed().as_secs_f64())))
            }
            _ => None,
        } };
        let (stats, failed) = (self.clone(), self.clone());
        let callbacks = Callbacks {
            on_stats: Box::new(move |s| stats.stats(s)),
            on_visible: Box::new(|shown| log!("app display: {}", if shown { "shown again" } else { "not shown: paused" })),
            on_error: Box::new(move |message| {
                let failed = failed.clone();
                glib::MainContext::default().invoke(move || failed.ended(Some(message)));
            }),
        };
        let channel = state.channel.clone().unwrap();
        match Streamer::new(source, channel, (width, height), fps, bitrate_for(options.bitrate, (width, height)),
                            &encoder, input, callbacks) {
            Ok(streamer) => {
                if let Err(e) = streamer.start() {
                    drop(state);
                    return self.ended(Some(e.to_string()));
                }
                state.streamer = Some(streamer);
                state.started = true;
            }
            Err(e) => {
                drop(state);
                return self.ended(Some(e.message().to_owned()));
            }
        }
        if source.is_some() && width >= HIDPI_WIDTH {
            // The virtual monitor shows in the display configuration a moment after its stream.
            let me = self.clone();
            let tries = std::cell::Cell::new(5);
            glib::timeout_add_local(Duration::from_millis(300), move || {
                if me.stopped() || matches!(monitor::set_virtual_monitor_scale(2.0), Ok(true) | Err(_)) || tries.get() <= 1 {
                    return glib::ControlFlow::Break;
                }
                tries.set(tries.get() - 1);
                glib::ControlFlow::Continue
            });
        }
    }

    /// From the listening thread: the app's report of the last second.
    fn stats(&self, stats: streamer::Stats) {
        let reports = self.0.reports.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some((before, now)) = stats.changed {
            log!("app display: {before} -> {now} kbit/s, as the network takes");
        }
        if stats.lost > 0 || stats.skipped > 0 || reports % STATS_EVERY == 1 {
            log!("app display: {}", streamer::describe(&stats));
        }
    }

    fn ended(&self, error: Option<String>) {
        if self.0.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let started = self.0.state.lock().unwrap().started;
        self.teardown();
        (self.0.on_ended)(error, started);
    }

    fn teardown(&self) {
        let (streamer, monitor) = {
            let mut state = self.0.state.lock().unwrap();
            state.channel = None;
            (state.streamer.take(), state.monitor.take())
        };
        if let Some(mut streamer) = streamer {
            streamer.stop();
        }
        if let Some(monitor) = monitor {
            monitor.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_at_the_tablet_s_proportions() {
        let found = sizes((2880, 1800));
        assert_eq!(found[0], (2880, 1800));
        assert!(found.contains(&(1920, 1200)) && found.contains(&(2560, 1600)));
        assert_eq!(auto_size((1800, 2880)), (1920, 1200));
        assert_eq!(bitrate_for(20000, (2560, 1600)), 35555);
        assert_eq!(bitrate_for(20000, (1280, 800)), 20000);
    }
}
