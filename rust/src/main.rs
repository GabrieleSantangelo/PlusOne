//! Plus One, in Rust: the same app as plusone/ (Python), its wire protocol
//! unchanged, ported a part at a time on the rust branch.
//!
//!   plus-one-rs ping          finds the tablet in use (mDNS), links, pings
//!   plus-one-rs bench-send    seals and sends frames as the desktop does

mod link;

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
        _ => {
            eprintln!("plus-one-rs {}: ping", env!("CARGO_PKG_VERSION"));
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
