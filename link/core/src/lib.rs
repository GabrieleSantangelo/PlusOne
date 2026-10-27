//! Plus One's link (docs/LINK.md): one session with each paired device,
//! over whatever path works, and channels in it for the features. The same
//! code on the PC (plus-one-link) and in the app (through JNI).
//!
//! A feature opens a channel, or takes one the other side opens, and gets
//! its end as a file descriptor: a message socket (one write, one
//! message). It never sees the path, the keys or the other channels.

pub mod frame;
pub mod identity;
pub mod mdns;
pub mod node;
pub mod pace;
pub mod relay;
pub mod seal;
pub mod session;
pub mod stream;
pub mod sys;

pub use identity::{fingerprint, Identity, Paired};
pub use node::{Event, Handler, Node};
pub use session::{Channel, Incoming, Session};
pub use stream::Stream;

/// How a channel's messages travel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Reliable and in order (requests, answers, settings).
    Control,
    /// Unreliable; only the newest state counts, never queued (the fingers,
    /// the pen).
    Input,
    /// Unreliable; late is lost (sound, video).
    Realtime,
}

impl Class {
    pub fn name(self) -> &'static str {
        match self {
            Class::Control => "control",
            Class::Input => "input",
            Class::Realtime => "realtime",
        }
    }

    pub fn parse(name: &str) -> Option<Class> {
        match name {
            "control" => Some(Class::Control),
            "input" => Some(Class::Input),
            "realtime" => Some(Class::Realtime),
            _ => None,
        }
    }
}

/// A line on stderr, with the time (the journal, under a service).
pub fn log(line: &str) {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    eprintln!("{:02}:{:02}:{:02} link: {line}", tm.tm_hour, tm.tm_min, tm.tm_sec);
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log(&format!($($arg)*)) };
}
