//! Plus One's link (docs/LINK.md): one session with each paired device,
//! over whatever path works, and channels in it for the features. The same
//! code on the PC (plus-one-link) and in the app (through JNI).
//!
//! A feature opens a channel, or takes one the other side opens, and gets
//! its end as a file descriptor: a message socket (one write, one
//! message). It never sees the path, the keys or the other channels.

mod cc;
#[cfg(fuzzing)]
pub use cc::fuzz as fuzz_controller;
mod dscp;
pub mod frame;
mod gate;
pub mod groups;
pub mod identity;
pub mod mdns;
pub mod node;
pub mod pace;
pub mod quic;
pub mod relay;
pub mod seal;
pub mod session;
pub mod stream;
pub mod sys;

pub use identity::{fingerprint, Identity, Paired};
pub use node::{Event, Handler, Node};
pub use session::{Channel, Incoming, PathInfo, PathState, Session};
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
    /// Realtime in groups (a video's frames): each message the feature
    /// writes starts with session::grouped::HEAD's bytes (the group, whether
    /// it is a key one and whether this is its last: Channel::send_in_group),
    /// which the link takes off. On a QUIC path whose other side reads groups
    /// (its hello), a group goes on a QUIC stream of its own: what is lost is
    /// sent again within the controller's rate, and the group is reset once
    /// late or superseded (groups.rs); else each message as Realtime. The
    /// other side's end is a Realtime one: its feature sees the messages as
    /// they were written.
    Video,
}

impl Class {
    pub fn name(self) -> &'static str {
        match self {
            Class::Control => "control",
            Class::Input => "input",
            Class::Realtime => "realtime",
            Class::Video => "video",
        }
    }

    /// What the other side is told the channel is: Video is Realtime to it
    /// (a peer from before groups knows no other).
    pub fn wire(self) -> &'static str {
        match self {
            Class::Video => "realtime",
            other => other.name(),
        }
    }

    pub fn parse(name: &str) -> Option<Class> {
        match name {
            "control" => Some(Class::Control),
            "input" => Some(Class::Input),
            "realtime" => Some(Class::Realtime),
            "video" => Some(Class::Video),
            _ => None,
        }
    }
}

/// Video's groups on QUIC streams and the session's messages on a stream of
/// their own (groups.rs; q3, option B), said in each hello and used where
/// both say so: on unless PLUSONE_LINK_VIDEO_STREAMS=0 (a live A/B against
/// the way before them, with no build: either side off, both are).
pub(crate) fn video_streams() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("PLUSONE_LINK_VIDEO_STREAMS").map_or(true, |v| v.trim() != "0");
        if !on {
            crate::log!("video streams off (PLUSONE_LINK_VIDEO_STREAMS=0): video as datagrams, as before them");
        }
        on
    })
}

/// A line on stderr, with the time (the journal, under a service).
#[cfg(not(target_os = "android"))]
pub fn log(line: &str) {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    eprintln!("{:02}:{:02}:{:02} link: {line}", tm.tm_hour, tm.tm_min, tm.tm_sec);
}

/// A line in the app's log (logcat, as the app's own: PlusOne, "link: ");
/// an app's stderr goes nowhere.
#[cfg(target_os = "android")]
pub fn log(line: &str) {
    #[link(name = "log")]
    extern "C" {
        fn __android_log_write(priority: libc::c_int, tag: *const libc::c_char, text: *const libc::c_char) -> libc::c_int;
    }
    const INFO: libc::c_int = 4;
    let text = std::ffi::CString::new(format!("link: {line}").replace('\0', " ")).unwrap_or_default();
    unsafe { __android_log_write(INFO, c"PlusOne".as_ptr(), text.as_ptr()) };
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log(&format!($($arg)*)) };
}
