//! Plus One, in Rust: the same app as plusone/ (Python), its wire protocol
//! unchanged, ported a part at a time on the rust branch.
//!
//!   plus-one-rs ping          finds the tablet in use (mDNS), links, pings
//!   plus-one-rs display [--size WxH] [--fps N] [--bitrate KBPS] [--encoder E]
//!                       [--test] [--no-input] [--seconds N]
//!                       [--at HOST:PORT]
//!                             the desktop on the tablet in use, as plus-one --cli
//!   plus-one-rs speakers [--seconds N] [--name SINK]
//!                             the tablet's speakers as a PipeWire sink, as the app's
//!   plus-one-rs fake-app DIR [--port P] [--screen WxH] [--seconds N]
//!                             a tablet of pretend, to measure the PC's side (bench.rs)
//!   plus-one-rs bench-av --at HOST:PORT [--seconds N] [--bitrate KBPS] [--size WxH]
//!                             a test pattern and a test tone to it at once

// The port is underway: what the next parts use (pairing from the window,
// a link's shutdown, the GUI's discovery) is here before them.
#![allow(dead_code)]

mod bench;
mod devices;
mod display;
mod link;
mod pcm;

use link::Source;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A line in the log, as plusone.log: the time, then what happened.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        let now = glib::DateTime::now_local().map(|t| t.format("%H:%M:%S").map(|s| s.to_string()).unwrap_or_default())
            .unwrap_or_default();
        println!("{} {}", now, format!($($arg)*));
    }};
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("ping") => ping(),
        Some("display") => show_desktop(&args[2..]),
        Some("fake-app") => bench::fake_app(&args[2..]),
        Some("speakers") => speakers(&args[2..]),
        Some("bench-av") => bench_av(&args[2..]),
        _ => {
            eprintln!("plus-one-rs {}: ping | display [options]", env!("CARGO_PKG_VERSION"));
            std::process::exit(2);
        }
    }
}

/// The tablet in use, found as the app finds it, linked and pinged.
fn ping() {
    let main = glib::MainLoop::new(None, false);
    let nearby = link::Nearby::new(|| {});
    nearby.start();
    let found: Arc<dyn Source> = Arc::new(link::Discovery::new(vec![Arc::new(nearby.view())]));
    let tablets = link::Tablets::new(None, Some(found.clone()), link::Paired::default());
    let quit = main.clone();
    // A few seconds for mDNS, then the link on a thread of its own.
    glib::timeout_add_seconds_local_once(3, move || {
        std::thread::spawn(move || {
            let Some(tablet) = tablets.active() else {
                log!("no tablet paired");
                return quit.quit();
            };
            let targets = tablets.targets(&tablet);
            log!("{} at {:?}, app {:?}", tablet.name, targets.iter().map(ToString::to_string).collect::<Vec<_>>(),
                 found.app_version(tablet.app_id()));
            let t0 = Instant::now();
            match link::connect(&targets, Some(tablet.app_id()), &tablets.paired, false) {
                Ok(mut tablet) => {
                    log!("linked to {} in {:.1} ms: {:?}", tablet.name, t0.elapsed().as_secs_f64() * 1e3,
                         tablet.hello.get("app"));
                    for _ in 0..5 {
                        let t = Instant::now();
                        match tablet.request(&msg!({"type": "ping"}), None, link::wire::MAX_HEADER) {
                            Ok(reply) => log!("ping -> {:?} in {:.1} ms", reply.get("type"), t.elapsed().as_secs_f64() * 1e3),
                            Err(e) => log!("ping: {e}"),
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    let _ = tablet.send(&msg!({"type": "bye"}), None);
                }
                Err(e) => log!("no link: {e}"),
            }
            quit.quit();
        });
    });
    main.run();
}

/// The desktop on the tablet in use, until it ends, Ctrl+C, or --seconds.
fn show_desktop(args: &[String]) {
    gst::init().expect("GStreamer");
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let options = display::Options {
        size: value("--size").and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        }),
        fps: value("--fps").and_then(|v| v.parse().ok()).unwrap_or(60),
        bitrate: value("--bitrate").and_then(|v| v.parse().ok()).unwrap_or(20000),
        encoder: value("--encoder").unwrap_or_else(|| "auto".into()),
        touch: !args.iter().any(|a| a == "--no-input"),
        test: args.iter().any(|a| a == "--test"),
    };
    let seconds: Option<u32> = value("--seconds").and_then(|v| v.parse().ok());
    let at: Option<link::Target> = value("--at").and_then(|a| {
        let (host, port) = a.rsplit_once(':')?;
        Some(link::Target::Tcp(host.to_owned(), port.parse().ok()?))
    });
    let main = glib::MainLoop::new(None, false);
    let nearby = link::Nearby::new(|| {});
    nearby.start();
    let found: Arc<dyn Source> = Arc::new(link::Discovery::new(vec![Arc::new(nearby.view())]));
    let tablets = link::Tablets::new(None, Some(found), link::Paired::default());
    let session: std::rc::Rc<std::cell::RefCell<Option<display::Session>>> = Default::default();
    let options = std::cell::Cell::new(Some(options));
    let (quit, started) = (main.clone(), session.clone());
    // A few seconds for mDNS (where the app was last otherwise).
    glib::timeout_add_seconds_local_once(2, move || {
        let Some(tablet) = tablets.active() else {
            log!("no tablet paired");
            return quit.quit();
        };
        let targets = at.clone().map(|t| vec![t]).unwrap_or_else(|| tablets.targets(&tablet));
        let done = quit.clone();
        *started.borrow_mut() = Some(display::Session::start(targets, tablet.app_id().to_owned(), options.take().unwrap(),
            move |error, _started| {
                if let Some(error) = error {
                    log!("app display ended: {error}");
                }
                done.quit();
            }));
    });
    let stop = {
        let (session, quit) = (session.clone(), main.clone());
        move || {
            if let Some(session) = session.borrow().as_ref() {
                session.stop();
            }
            // The link's thread tells the app, within its tick.
            let quit = quit.clone();
            glib::timeout_add_local_once(Duration::from_millis(800), move || quit.quit());
        }
    };
    let on_signal = stop.clone();
    glib::unix_signal_add_local(libc::SIGINT, move || {
        on_signal();
        glib::ControlFlow::Break
    });
    if let Some(seconds) = seconds {
        glib::timeout_add_seconds_local_once(seconds, stop);
    }
    main.run();
}

fn option(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// Where the tablet in use is, from discovery (or --at).
fn whereabouts(at: Option<link::Target>) -> (link::Nearby, devices::Where) {
    let nearby = link::Nearby::new(|| {});
    nearby.start();
    let found: Arc<dyn Source> = Arc::new(link::Discovery::new(vec![Arc::new(nearby.view())]));
    let tablets = link::Tablets::new(None, Some(found), link::Paired::default());
    let whereabouts: devices::Where = Arc::new(move || {
        let tablet = tablets.active()?;
        let targets = at.clone().map(|t| vec![t]).unwrap_or_else(|| tablets.targets(&tablet));
        Some((targets, tablet.app_id().to_owned()))
    });
    (nearby, whereabouts)
}

fn run_for(seconds: Option<u32>, at_end: impl Fn() + 'static) {
    let main = glib::MainLoop::new(None, false);
    let at_end = std::rc::Rc::new(at_end);
    let stop = {
        let (main, at_end) = (main.clone(), at_end.clone());
        move || {
            at_end();
            let main = main.clone();
            glib::timeout_add_local_once(Duration::from_millis(1500), move || main.quit());
        }
    };
    let on_signal = stop.clone();
    glib::unix_signal_add_local(libc::SIGINT, move || {
        on_signal();
        glib::ControlFlow::Break
    });
    if let Some(seconds) = seconds {
        glib::timeout_add_seconds_local_once(seconds, stop);
    }
    main.run();
}

/// The tablet's speakers, as the app has them: a sink while on, the tablet
/// asked while a program plays into it.
fn speakers(args: &[String]) {
    gst::init().expect("GStreamer");
    let name = option(args, "--name").unwrap_or_else(|| "plusone_speakers_rs".into());
    let (_nearby, whereabouts) = whereabouts(None);
    let speakers = devices::Speakers::new(&name, "Tablet speakers (Plus One, Rust)", whereabouts);
    if let Err(e) = speakers.enable() {
        return log!("speakers: {e}");
    }
    let seconds = option(args, "--seconds").and_then(|v| v.parse().ok());
    run_for(seconds, move || speakers.disable());
}

/// A test pattern and a test tone to the tablet at --at at once, as the app
/// sends the desktop and the speakers: for the fake app to measure.
fn bench_av(args: &[String]) {
    gst::init().expect("GStreamer");
    let at = option(args, "--at").and_then(|a| {
        let (host, port) = a.rsplit_once(':')?;
        Some(link::Target::Tcp(host.to_owned(), port.parse().ok()?))
    });
    let (_nearby, whereabouts) = whereabouts(at);
    let feed = devices::Feed::new("speakers", whereabouts.clone(),
        || pcm::PcmSender::new("audiotestsrc is-live=true wave=sine freq=440 volume=0.3 samplesperbuffer=240")
            .map(|s| Box::new(s) as Box<dyn devices::Receiver>), || {}, |_| {});
    feed.start();
    let (targets, id) = whereabouts().expect("a paired tablet");
    let size = option(args, "--size").and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    });
    let options = display::Options { size, fps: 60, bitrate: option(args, "--bitrate").and_then(|v| v.parse().ok()).unwrap_or(20000),
                                     encoder: "auto".into(), touch: false, test: true };
    let session = display::Session::start(targets, id, options, |error, _| {
        if let Some(error) = error {
            log!("app display ended: {error}");
        }
    });
    let seconds = option(args, "--seconds").and_then(|v| v.parse().ok());
    run_for(seconds, move || {
        session.stop();
        feed.stop();
    });
}
