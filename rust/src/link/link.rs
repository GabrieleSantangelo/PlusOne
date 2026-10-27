//! Links: authenticated connections to the paired tablets' app (link.py).
//!
//! The app listens on the network at APP_PORT (or where it is announced). A
//! link is a TCP stream secured with TLS 1.3, each side presenting its
//! self-signed certificate (trust.rs), and a hello each way. A tablet not
//! paired yet is paired only when the user asked (pair()); otherwise only a
//! paired tablet's app gets a link, and a certificate other than the paired
//! one where that tablet is announced is an Impostor.

use super::trust::{self, commitment, fingerprint, identity, pairing_code, Paired, MAX_NAME};
use super::wire::{self, Message, MAX_HEADER, UNPAIRED_HEADER};
use crate::log;
use crate::msg;
use openssl::ssl::{SslConnector, SslMethod, SslStream, SslVerifyMode, SslVersion};
use serde_json::Value;
use std::io;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

pub const PROTOCOL: i64 = 3;
pub const APP_PORT: u16 = 47236;  // the app's (LinkServer.kt PORT), unless it was taken
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const PAIRING_TIMEOUT: Duration = Duration::from_secs(120);  // for the user to compare the codes
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);
pub const LINK_TIMEOUT: Duration = Duration::from_secs(20);  // the tablet acknowledges what was sent within
pub const HANDSHAKE_FOR: Duration = Duration::from_secs(20);  // TLS, hello and pairing messages
pub const KEEPALIVE: Duration = Duration::from_secs(5);  // a session's pings (the app lets go of a silent PC)

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The app speaks another version of the protocol.
    Incompatible(String),
    PairingRefused(String),
    /// At an address announced for a paired tablet, another certificate.
    Impostor(String),
    /// The app did not do what was asked: "off", "permission", "background", "busy", "unavailable".
    Refused(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Incompatible(m) | Error::Impostor(m) => write!(f, "{m}"),
            Error::PairingRefused(name) => write!(f, "pairing with {name} refused"),
            Error::Refused(reason) => write!(f, "refused: {reason}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

impl From<openssl::error::ErrorStack> for Error {
    fn from(e: openssl::error::ErrorStack) -> Error {
        Error::Io(io::Error::other(e))
    }
}

/// Where the app is: (host, port) on the network. (Another transport would
/// be another variant: transport.py's (kind, ...).)
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Target {
    Tcp(String, u16),
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Target::Tcp(host, port) => write!(f, "{host}:{port}"),
        }
    }
}

fn open_stream(target: &Target, timeout: Duration) -> io::Result<TcpStream> {
    match target {
        Target::Tcp(host, port) => {
            let mut error = io::Error::new(io::ErrorKind::NotFound, "no address");
            for address in (host.as_str(), *port).to_socket_addrs()? {
                match TcpStream::connect_timeout(&address, timeout) {
                    Ok(stream) => {
                        stream.set_nodelay(true)?;  // small messages at once
                        return Ok(stream);
                    }
                    Err(e) => error = e,
                }
            }
            Err(error)
        }
    }
}

/// A link to a paired tablet's app. name: the tablet's; fingerprint: its
/// certificate's; hello: what its app said of itself.
pub struct Link {
    stream: SslStream<TcpStream>,
    raw: TcpStream,  // the same socket: its timeouts, and shutdown() from another thread
    pub name: String,
    pub fingerprint: String,
    pub hello: Message,
}

impl Link {
    /// The app's address.
    pub fn peer(&self) -> io::Result<IpAddr> {
        Ok(self.raw.peer_addr()?.ip())
    }

    pub fn send(&mut self, header: &Message, payload: Option<&[u8]>) -> io::Result<()> {
        wire::send(&mut self.stream, header, payload)
    }

    pub fn receive(&mut self, limit: usize) -> io::Result<Message> {
        wire::receive(&mut self.stream, limit)
    }

    /// Whether a message starts arriving within timeout.
    pub fn readable(&mut self, timeout: Duration) -> io::Result<bool> {
        if self.stream.ssl().pending() > 0 {
            return Ok(true);
        }
        let mut fd = libc::pollfd { fd: self.raw.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let n = unsafe { libc::poll(&mut fd, 1, timeout.as_millis().min(i32::MAX as u128) as i32) };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted { Ok(false) } else { Err(e) };
        }
        Ok(n > 0)
    }

    /// header (and its payload) to the app, and its answer; Refused if it said no.
    pub fn request(&mut self, header: &Message, payload: Option<&[u8]>, limit: usize) -> Result<Message, Error> {
        self.send(header, payload)?;
        let reply = self.receive(limit)?;
        if reply.get("type").and_then(Value::as_str) == Some("refused") {
            let reason = reply.get("reason").and_then(Value::as_str).unwrap_or("").to_owned();
            return Err(Error::Refused(reason));
        }
        Ok(reply)
    }

    /// While a session lasts, until until() is true: pings every so often
    /// (the app lets go of a silent PC), reads what the app says (its
    /// hanging up ends it, an error), and check() each tick, which may end
    /// the session with an error of its own.
    pub fn hold(
        &mut self,
        until: impl Fn() -> bool,
        mut check: impl FnMut() -> io::Result<()>,
        tick: Duration,
    ) -> io::Result<()> {
        let mut pinged = Instant::now();
        while !until() {
            check()?;
            if self.readable(tick)? {
                self.receive(MAX_HEADER)?;
            }
            if pinged.elapsed() > KEEPALIVE {
                self.send(&msg!({"type": "ping"}), None)?;
                pinged = Instant::now();
            }
        }
        Ok(())
    }

    pub fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.raw.set_read_timeout(timeout)?;
        self.raw.set_write_timeout(timeout)
    }

    /// A handle that ends the link from another thread: whatever waits on it wakes.
    pub fn shutdown_handle(&self) -> io::Result<TcpStream> {
        self.raw.try_clone()
    }
}

/// The question to the user while pairing: (tablet name, code) -> accepted.
pub type AskPairing<'a> = &'a dyn Fn(&str, &str) -> bool;

fn handshake(
    target: &Target,
    paired: &Paired,
    ask: Option<AskPairing>,
    expect: Option<&str>,
    any_version: bool,
) -> Result<Link, Error> {
    let me = identity(None)?;
    let mut tls = SslConnector::builder(SslMethod::tls_client())?;
    tls.set_min_proto_version(Some(SslVersion::TLS1_3))?;
    // No CA vouches for either side: the app's certificate is checked
    // against the paired list (or the user compares the code).
    tls.set_verify(SslVerifyMode::NONE);
    tls.set_certificate_chain_file(&me.cert)?;
    tls.set_private_key_file(&me.key, openssl::ssl::SslFiletype::PEM)?;
    let raw = open_stream(target, CONNECT_TIMEOUT)?;
    raw.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    raw.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    let config = tls.build().configure()?.use_server_name_indication(false).verify_hostname(false);
    let stream = config.connect("", raw.try_clone()?).map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
    let deadline = Instant::now() + HANDSHAKE_FOR;
    let theirs = stream.ssl().peer_certificate().ok_or_else(|| io::Error::other("no certificate"))?.to_der()?;
    let fp = fingerprint(&theirs);
    let mut link = Link { stream, raw, name: String::new(), fingerprint: fp.clone(), hello: Message::new() };
    // A message of the handshake: small, and all of them in time.
    let small = |link: &mut Link| -> io::Result<Message> {
        let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(100));
        link.raw.set_read_timeout(Some(left))?;
        link.receive(UNPAIRED_HEADER)
    };
    let known_list = paired.all();
    let pinned: Vec<&String> = known_list.keys().filter(|p| expect.is_some_and(|e| p.starts_with(e))).collect();
    if !pinned.is_empty() && !pinned.contains(&&fp) {
        return Err(Error::Impostor(format!("an app at {target} announced as a paired tablet, with another certificate")));
    }
    let known = known_list.contains_key(&fp);
    link.set_timeout(Some(IO_TIMEOUT))?;
    link.send(&msg!({"type": "hello", "name": trust::hostname(), "version": PROTOCOL, "paired": known,
                     "pair": ask.is_some()}), None)?;
    let hello = small(&mut link)?;
    let name: String = hello.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or("tablet")
        .chars().take(MAX_NAME).collect();
    let both_know = known && hello.get("paired").and_then(Value::as_bool).unwrap_or(false);
    if hello.get("version").and_then(Value::as_i64) != Some(PROTOCOL) {
        if !any_version {
            let theirs = hello.get("version").cloned().unwrap_or(Value::Null);
            return Err(Error::Incompatible(format!("{name} speaks version {theirs}, this PC {PROTOCOL}")));
        }
        if !both_know {
            return Err(Error::PairingRefused(name));  // never paired across versions
        }
    }
    if !both_know {
        let Some(ask) = ask else { return Err(Error::PairingRefused(name)) };
        // The app commits to its number, learns ours, then shows its own.
        let commit = small(&mut link)?;
        let mut nonce = [0u8; 16];
        openssl::rand::rand_bytes(&mut nonce)?;
        link.send(&msg!({"type": "pair-nonce", "nonce": trust::hex(&nonce)}), None)?;
        let reveal = small(&mut link)?;
        let their_nonce = reveal.get("nonce").and_then(Value::as_str).and_then(trust::unhex).unwrap_or_default();
        if commit.get("type").and_then(Value::as_str) != Some("pair-commit")
            || their_nonce.len() != 16
            || commit.get("commit").and_then(Value::as_str) != Some(&commitment(&their_nonce, &theirs, &me.der))
        {
            return Err(Error::PairingRefused(name));
        }
        let code = pairing_code(&me.der, &theirs, &nonce, &their_nonce);
        log!("link: pairing with {name}, code {code}");
        link.raw.set_read_timeout(Some(PAIRING_TIMEOUT + Duration::from_secs(10)))?;
        let accepted = ask(&name, &code);
        link.send(&msg!({"type": if accepted { "pair-ok" } else { "pair-no" }}), None)?;
        let answer = link.receive(UNPAIRED_HEADER)?;
        if !accepted || answer.get("type").and_then(Value::as_str) != Some("pair-ok") {
            return Err(Error::PairingRefused(name));
        }
        paired.add(&fp, &name)?;
    }
    link.set_timeout(Some(IO_TIMEOUT))?;  // the handshake's budget is no timeout for what follows
    link.name = name;
    link.hello = hello;
    Ok(link)
}

/// Blocking: pairs with the app at targets, ask(name, code) asking the user
/// to compare the code the tablet shows too. (fingerprint, name).
pub fn pair(targets: &[Target], ask: AskPairing, paired: &Paired) -> Result<(String, String), Error> {
    let mut error = Error::Io(io::Error::new(io::ErrorKind::NotFound, "no address for the tablet"));
    for target in targets {
        match handshake(target, paired, Some(ask), None, false) {
            Ok(mut link) => {
                let _ = link.send(&msg!({"type": "bye"}), None);
                return Ok((link.fingerprint, link.name));
            }
            Err(e @ (Error::Io(_) | Error::Impostor(_))) => error = e,
            Err(e) => return Err(e),
        }
    }
    Err(error)
}

/// Blocking: a Link to the paired tablet's app at the first of targets that
/// answers as it should. expect: the id the app is announced with.
pub fn connect(targets: &[Target], expect: Option<&str>, paired: &Paired, any_version: bool) -> Result<Link, Error> {
    let mut error = Error::Io(io::Error::new(io::ErrorKind::NotFound, "no address for the tablet"));
    for target in targets {
        match handshake(target, paired, None, expect, any_version) {
            Ok(link) => {
                // A tablet gone from the network answers nothing, not even a
                // reset: the kernel gives the link up after LINK_TIMEOUT
                // instead of TCP's quarter of an hour.
                let ms = LINK_TIMEOUT.as_millis() as libc::c_uint;
                unsafe {
                    libc::setsockopt(link.raw.as_raw_fd(), libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT,
                                     &ms as *const _ as *const libc::c_void, std::mem::size_of_val(&ms) as u32);
                }
                return Ok(link);
            }
            Err(Error::Impostor(m)) => {
                log!("link: {m}");
                error = Error::Impostor(m);
            }
            Err(e @ Error::Io(_)) => error = e,
            Err(e) => return Err(e),
        }
    }
    Err(error)
}

/// Where a target's (host, port) is, for the socket API.
pub fn socket_addr(target: &Target) -> Option<SocketAddr> {
    match target {
        Target::Tcp(host, port) => (host.as_str(), *port).to_socket_addrs().ok()?.next(),
    }
}
