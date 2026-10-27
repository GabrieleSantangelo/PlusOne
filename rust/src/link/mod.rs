//! Plus One's connectivity: everything between this PC and its paired
//! tablets, under the services that use it, which never open a socket of
//! their own (plusone/link in Python, link/ in the app).
//!
//! - discovery: which apps are around, where, of which version (mDNS);
//! - trust: this PC's identity, the paired tablets, the pairing code;
//! - tablets: the paired ones, the one in use, where to reach it;
//! - link: connect() gives a Link to a paired tablet's app (TLS 1.3):
//!   request() and its answer, hold() to keep a session's link alive;
//! - datagrams: a session's UDP, sealed both ways, for what must not wait;
//! - wire: the framed messages.

pub mod datagrams;
pub mod discovery;
#[allow(clippy::module_inception)]
pub mod link;
pub mod tablets;
pub mod trust;
pub mod wire;

pub use datagrams::Datagrams;
pub use discovery::{Discovery, Nearby, Source};
pub use link::{connect, pair, Error, Link, Target, APP_PORT, IO_TIMEOUT, KEEPALIVE, PROTOCOL};
pub use tablets::{Tablet, Tablets};
pub use trust::Paired;
