//! A Video channel's messages (lib.rs Class::Video): each starts with
//! HEAD's bytes, its flags and its group (a video's frame number), which
//! the session takes off. On a QUIC path whose other side reads groups (its
//! hello says "groups"), a group's messages go sealed as datagrams are, in
//! order, on a QUIC stream of their own (groups.rs); anywhere else each goes
//! as a Realtime message does (a datagram, or the stream's while UDP is not
//! proven). The other side's feature reads them as it reads datagrams.

use super::{Channel, Session, MAX_DATAGRAM};
use crate::sys;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering;

/// A Video message's head: its flags (IN_GROUP, LAST, KEY), then its group
/// (u32, big-endian).
pub const HEAD: usize = 5;
/// One of a group's messages (else a Realtime message of its own).
pub const IN_GROUP: u8 = 1;
/// The group's last message: its stream is finished.
pub const LAST: u8 = 2;
/// A key group (a keyframe's): ahead of all, and the older groups not all
/// through yet are given up when it begins (groups.rs).
pub const KEY: u8 = 4;

impl Channel {
    /// One message of group (a Video channel's: a frame's piece; the frame's
    /// number), whether the group is a key one (a keyframe) and whether this
    /// is its last; waiting for room.
    pub fn send_in_group(&self, group: u32, key: bool, last: bool, data: &[u8]) -> io::Result<()> {
        let mut message = Vec::with_capacity(HEAD + data.len());
        message.push(IN_GROUP | if last { LAST } else { 0 } | if key { KEY } else { 0 });
        message.extend_from_slice(&group.to_be_bytes());
        message.extend_from_slice(data);
        sys::send(self.fd.as_raw_fd(), &message, true).map(|_| ())
    }
}

impl Session {
    /// A Video channel's message, its head taken off: on its group's QUIC
    /// stream if the path takes them, else as Realtime (send_datagram).
    pub(super) fn send_grouped(&self, channel: u16, message: &[u8], sealed: &mut Vec<u8>) {
        let Some((head, body)) = message.split_first_chunk::<HEAD>() else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let group = u32::from_be_bytes([head[1], head[2], head[3], head[4]]);
        let current = self.current();
        let path = current.as_ref().filter(|path| path.udp_works() && path.quic.as_ref().is_some_and(|q| q.groups()));
        match (path, head[0] & IN_GROUP != 0) {
            (Some(path), true) if body.len() <= MAX_DATAGRAM => {
                path.realtime_us.store(path.us().max(1), Ordering::Relaxed);
                self.sealer.seal(channel, body, sealed);
                let quic = path.quic.as_ref().expect("a QUIC path");
                if !quic.send_in_group(channel, group, head[0] & KEY != 0, head[0] & LAST != 0, sealed) {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            _ => self.send_datagram(channel, body, sealed),
        }
    }

    /// The groups on the path now, if it is a QUIC one (groups.rs).
    pub fn group_stats(&self) -> Option<crate::groups::Stats> {
        self.current().and_then(|path| path.quic.as_ref().map(|quic| quic.group_stats()))
    }
}
