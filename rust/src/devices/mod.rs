//! The tablet's devices as the PC's (devices.py): the speakers so far.
//!
//! A device is a virtual PipeWire node made while it is on; the tablet is
//! asked for it only while a program uses it (known from the node's links,
//! pw-link, and named by pw-dump), through a Feed: a session with the
//! tablet in use on a thread of its own, made again when it drops.

use crate::link::{self, Paired, Target};
use crate::pcm::PcmSender;
use crate::{log, msg};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, BufReader};
use std::net::{IpAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const RETRY: Duration = Duration::from_secs(3);           // between attempts to reach the tablet while wanted
const REFUSED_RETRY: Duration = Duration::from_secs(10);  // after the app said no (the user may change their mind there)
const IN_USE_AFTER: Duration = Duration::from_millis(200);  // a program holds the device this long before the tablet is asked
const IDLE_AFTER: Duration = Duration::from_secs(3);      // without one this long, the tablet stops
const SILENT_FOR: Duration = Duration::from_secs(5);      // a session without media this long is made again

/// Where the tablet in use is: (its app's addresses, its app id).
pub type Where = Arc<dyn Fn() -> Option<(Vec<Target>, String)> + Send + Sync>;

/// A session's media, as the Feed sees it: its offer and the app's answer
/// (WebRTC's SDP, or the speakers' key and ports), whether it flows.
pub trait Receiver: Send {
    fn offer(&self) -> String;
    fn answer(&mut self, sdp: &str, peer: IpAddr) -> Result<(), String>;
    /// Grows while the media flows.
    fn flowing(&self) -> u64;
    fn failed(&self) -> Option<String>;
    fn close(&mut self);
}

impl Receiver for PcmSender {
    fn offer(&self) -> String {
        PcmSender::offer(self)
    }
    fn answer(&mut self, sdp: &str, peer: IpAddr) -> Result<(), String> {
        PcmSender::answer(self, sdp, peer)
    }
    fn flowing(&self) -> u64 {
        self.captured()
    }
    fn failed(&self) -> Option<String> {
        PcmSender::failed(self)
    }
    fn close(&mut self) {
        PcmSender::close(self)
    }
}

struct FeedInner {
    kind: String,
    wanted: AtomicBool,
    wake: (Mutex<()>, Condvar),
    link: Mutex<Option<TcpStream>>,  // the session's link, to end it from another thread
    whereabouts: Where,
    make: Box<dyn Fn() -> Result<Box<dyn Receiver>, String> + Send + Sync>,
    on_ready: Box<dyn Fn() + Send + Sync>,
    on_problem: Box<dyn Fn(&str) + Send + Sync>,
}

/// One device's session with the tablet in use, on a thread of its own while
/// wanted, made again when it drops. on_problem: "off", "permission",
/// "background", "unavailable", "away", "unpaired", "version".
#[derive(Clone)]
pub struct Feed(Arc<FeedInner>);

impl Feed {
    pub fn new(kind: &str, whereabouts: Where, make: impl Fn() -> Result<Box<dyn Receiver>, String> + Send + Sync + 'static,
               on_ready: impl Fn() + Send + Sync + 'static, on_problem: impl Fn(&str) + Send + Sync + 'static) -> Feed {
        Feed(Arc::new(FeedInner {
            kind: kind.to_owned(), wanted: AtomicBool::new(false), wake: Default::default(), link: Mutex::new(None),
            whereabouts, make: Box::new(make), on_ready: Box::new(on_ready), on_problem: Box::new(on_problem),
        }))
    }

    pub fn start(&self) {
        if self.0.wanted.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        std::thread::Builder::new().name(format!("plusone-{}", self.0.kind)).spawn(move || {
            while me.0.wanted.load(Ordering::SeqCst) {
                if let Some(delay) = me.session() {
                    let (lock, wake) = &me.0.wake;
                    let _ = wake.wait_timeout(lock.lock().unwrap(), delay);
                }
            }
            log!("{}: the tablet stopped", me.0.kind);
        }).unwrap();
    }

    pub fn stop(&self) {
        self.0.wanted.store(false, Ordering::SeqCst);
        self.0.wake.1.notify_all();
        if let Some(link) = self.0.link.lock().unwrap().take() {
            let _ = link.shutdown(std::net::Shutdown::Both);  // the session's thread wakes, and ends it
        }
    }

    fn wanted(&self) -> bool {
        self.0.wanted.load(Ordering::SeqCst)
    }

    /// One session with the tablet in use, until it ends: how long to wait
    /// before the next, or None.
    fn session(&self) -> Option<Duration> {
        let kind = self.0.kind.as_str();
        let Some((targets, id)) = (self.0.whereabouts)().filter(|(t, _)| !t.is_empty()) else {
            (self.0.on_problem)("away");
            return Some(RETRY);
        };
        let mut tablet = match link::connect(&targets, Some(&id), &Paired::default(), false) {
            Ok(tablet) => tablet,
            Err(link::Error::PairingRefused(e)) => {
                log!("{kind}: the tablet does not know this PC ({e})");
                (self.0.on_problem)("unpaired");
                return Some(REFUSED_RETRY);
            }
            Err(link::Error::Incompatible(e)) => {
                log!("{kind}: {e}");
                (self.0.on_problem)("version");
                return Some(REFUSED_RETRY);
            }
            Err(e) => {
                if !self.wanted() {
                    return None;
                }
                log!("{kind}: no tablet: {e}");
                (self.0.on_problem)("away");
                return Some(RETRY);
            }
        };
        *self.0.link.lock().unwrap() = tablet.shutdown_handle().ok();
        if !self.wanted() {
            return None;  // stopped while it connected
        }
        let mut receiver = match (self.0.make)() {
            Ok(receiver) => receiver,
            Err(e) => {
                log!("{kind}: {e}");
                (self.0.on_problem)("unavailable");
                return Some(REFUSED_RETRY);
            }
        };
        let result = (|| -> Result<(), link::Error> {
            let mut request = msg!({"type": kind});
            request.insert("sdp".into(), Value::String(receiver.offer()));
            let reply = tablet.request(&request, None, link::wire::MAX_HEADER)?;
            if reply.get("type").and_then(Value::as_str) != Some("answer") {
                return Err(link::Error::Io(std::io::Error::other(format!("the app answered {:?}", reply.get("type")))));
            }
            let peer = tablet.peer()?;
            receiver.answer(reply.get("sdp").and_then(Value::as_str).unwrap_or(""), peer)
                .map_err(|e| link::Error::Io(std::io::Error::other(e)))?;
            log!("{kind}: {}", if kind == "speakers" { "to the tablet, as it is" } else { "from the tablet, over WebRTC" });
            (self.0.on_ready)();
            // While wanted, the link up and the media flowing.
            let (mut heard, mut last) = (0, Instant::now());
            tablet.hold(|| !self.wanted(), || {
                if let Some(problem) = receiver.failed() {
                    return Err(std::io::Error::other(problem));
                }
                let flowing = receiver.flowing();
                if flowing > heard {
                    (heard, last) = (flowing, Instant::now());
                } else if last.elapsed() > SILENT_FOR {
                    return Err(std::io::Error::other(format!("nothing for {} s", SILENT_FOR.as_secs())));
                }
                Ok(())
            }, Duration::from_secs(1))?;
            Ok(())
        })();
        receiver.close();
        self.0.link.lock().unwrap().take();
        match result {
            Ok(()) => None,
            Err(link::Error::Refused(reason)) => {
                log!("{kind}: the tablet said no ({reason})");
                (self.0.on_problem)(&reason);
                // Turned off or not allowed there: the user has to act; else soon.
                Some(if reason == "off" || reason == "permission" { REFUSED_RETRY } else { RETRY })
            }
            Err(_) if !self.wanted() => None,
            Err(e) => {
                log!("{kind}: no tablet: {e}");
                (self.0.on_problem)("away");
                Some(RETRY)
            }
        }
    }
}

/// Links from pw-link --monitor --links --output, line by line: ('+',
/// (output port, input port)) for one made, '-' removed, '=' there already
/// (those come as a port's line, then "=   |-> input" lines).
#[derive(Default)]
pub struct LinkParser {
    port: Option<String>,
}

impl LinkParser {
    pub fn parse(&mut self, line: &str) -> Option<(char, (String, String))> {
        let line = line.trim_end();
        let (sign, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();
        let sign = match sign {
            "=" | "+" | "-" => sign.chars().next().unwrap(),
            _ => return None,
        };
        if let Some(input) = rest.strip_prefix("|->") {
            return self.port.clone().map(|port| (sign, (port, input.trim().to_owned())));
        }
        if let Some((source, target)) = rest.split_once(" -> ") {
            return Some((sign, (source.trim().to_owned(), target.trim().to_owned())));
        }
        self.port = Some(rest.to_owned());
        None
    }
}

/// pw-dump's objects: one JSON array, and while the graph changes more
/// arrays after it, of what changed: each object as last told, those
/// removed (info null) left out.
pub fn pw_objects(text: &str) -> Vec<Value> {
    let mut objects: BTreeMap<i64, Value> = BTreeMap::new();
    for document in serde_json::Deserializer::from_str(text).into_iter::<Value>().map_while(Result::ok) {
        if let Value::Array(items) = document {
            for o in items {
                let id = o.get("id").and_then(Value::as_i64).unwrap_or(-1);
                objects.insert(id, o);
            }
        }
    }
    objects.into_values().filter(|o| !o.get("info").is_none_or(Value::is_null)).collect()
}

/// The programs whose streams play into the sink named name, by name; one
/// entry per stream.
pub fn players(objects: &[Value], name: &str) -> Vec<String> {
    let props = |o: &Value| o.pointer("/info/props").cloned().unwrap_or(Value::Null);
    let nodes: BTreeMap<i64, Value> = objects.iter()
        .filter(|o| o.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Node"))
        .map(|o| (o.get("id").and_then(Value::as_i64).unwrap_or(-1), props(o))).collect();
    let sink: HashSet<i64> = nodes.iter()
        .filter(|(_, p)| p.get("node.name").and_then(Value::as_str) == Some(name)).map(|(id, _)| *id).collect();
    objects.iter()
        .filter(|o| o.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Link"))
        .filter(|o| o.pointer("/info/input-node-id").and_then(Value::as_i64).is_some_and(|id| sink.contains(&id)))
        .map(|o| {
            let source = o.pointer("/info/output-node-id").and_then(Value::as_i64).and_then(|id| nodes.get(&id));
            source.and_then(|p| p.get("application.name").or_else(|| p.get("node.name")))
                .and_then(Value::as_str).unwrap_or("?").to_owned()
        }).collect()
}

fn pactl(args: &[&str]) -> std::io::Result<String> {
    let out = Command::new("pactl").args(args).output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_owned()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A node left by a Plus One that did not end cleanly.
fn unload_stale(name: &str) {
    let Ok(modules) = pactl(&["list", "modules", "short"]) else { return };
    for line in modules.lines() {
        if line.contains(&format!("sink_name={name} ")) || line.ends_with(&format!("sink_name={name}")) {
            if let Some(id) = line.split_whitespace().next() {
                let _ = pactl(&["unload-module", id]);
            }
        }
    }
}

struct SpeakersInner {
    name: String,
    description: String,
    whereabouts: Where,
    module: Mutex<Option<String>>,
    links: Mutex<HashSet<(String, String)>>,
    watch: Mutex<Option<Child>>,
    in_use: Mutex<bool>,
    generation: AtomicU64,
    feed: Mutex<Option<Feed>>,
    on: AtomicBool,
}

/// The tablet's speakers: a virtual PipeWire sink, which Plus One captures
/// and sends to the tablet as it is while a program plays into it.
#[derive(Clone)]
pub struct Speakers(Arc<SpeakersInner>);

impl Speakers {
    pub fn new(name: &str, description: &str, whereabouts: Where) -> Speakers {
        Speakers(Arc::new(SpeakersInner {
            name: name.into(), description: description.into(), whereabouts, module: Mutex::new(None),
            links: Mutex::default(), watch: Mutex::new(None), in_use: Mutex::new(false),
            generation: AtomicU64::new(0), feed: Mutex::new(None), on: AtomicBool::new(false),
        }))
    }

    pub fn enable(&self) -> Result<(), String> {
        if self.0.module.lock().unwrap().is_some() {
            return Ok(());
        }
        unload_stale(&self.0.name);
        // No rate of its own: it follows the graph's (Bluetooth headphones run
        // theirs at 44.1 kHz, and a 48 kHz source went silent in it).
        let module = pactl(&["load-module", "module-null-sink", &format!("sink_name={}", self.0.name),
                             "media.class=Audio/Sink", "channels=2", "channel_map=front-left,front-right",
                             &format!("sink_properties=device.description='{}'", self.0.description)])
            .map_err(|e| format!("no PipeWire node: {e}"))?;
        *self.0.module.lock().unwrap() = Some(module.trim().to_owned());
        self.0.on.store(true, Ordering::SeqCst);
        self.watch_links()?;
        log!("speakers: on, as {}", self.0.name);
        Ok(())
    }

    pub fn disable(&self) {
        self.0.on.store(false, Ordering::SeqCst);
        self.end();
        if let Some(mut watch) = self.0.watch.lock().unwrap().take() {
            let _ = watch.kill();
            let _ = watch.wait();
        }
        if let Some(module) = self.0.module.lock().unwrap().take() {
            if let Err(e) = pactl(&["unload-module", &module]) {
                log!("speakers: its node not removed: {e}");
            }
        }
        self.0.links.lock().unwrap().clear();
        *self.0.in_use.lock().unwrap() = false;
    }

    /// pw-link tells each link made or removed; a program playing into the
    /// sink is a link into its playback ports.
    fn watch_links(&self) -> Result<(), String> {
        let mut child = Command::new("pw-link").args(["--monitor", "--links", "--output"])
            .stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| e.to_string())?;
        let stdout = child.stdout.take().unwrap();
        *self.0.watch.lock().unwrap() = Some(child);
        let me = self.clone();
        std::thread::spawn(move || {
            let mut parser = LinkParser::default();
            let ours = format!("{}:playback_", me.0.name);
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Some((sign, link)) = parser.parse(&line) else { continue };
                if !link.1.starts_with(&ours) {
                    continue;
                }
                {
                    let mut links = me.0.links.lock().unwrap();
                    if sign == '-' { links.remove(&link); } else { links.insert(link); }
                }
                me.who_plays();
            }
        });
        Ok(())
    }

    /// Whether a program plays into it, from what pw-dump says of the
    /// streams linked to it.
    fn who_plays(&self) {
        if self.0.links.lock().unwrap().is_empty() {
            return self.used(false);
        }
        let names = Command::new("pw-dump").stderr(Stdio::null()).output().ok()
            .map(|out| players(&pw_objects(&String::from_utf8_lossy(&out.stdout)), &self.0.name));
        // Not known: as if a program played.
        self.used(names.is_none_or(|names| !names.is_empty()));
    }

    /// A program took it, or let it go: the tablet follows, a little later.
    fn used(&self, in_use: bool) {
        let generation = self.0.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let me = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(if in_use { IN_USE_AFTER } else { IDLE_AFTER });
            if me.0.generation.load(Ordering::SeqCst) != generation || !me.0.on.load(Ordering::SeqCst) {
                return;
            }
            let mut current = me.0.in_use.lock().unwrap();
            if *current == in_use {
                return;
            }
            *current = in_use;
            drop(current);
            if in_use {
                log!("speakers: a program uses it");
                me.begin();
            } else {
                log!("speakers: no program uses it");
                me.end();
            }
        });
    }

    fn begin(&self) {
        let source = format!("pipewiresrc target-object={} do-timestamp=true \
                              stream-properties=\"props,stream.capture.sink=true,node.latency=240/48000,\
                              node.name=plusone-speakers-feed\"", self.0.name);
        let feed = Feed::new("speakers", self.0.whereabouts.clone(),
                             move || PcmSender::new(&source).map(|s| Box::new(s) as Box<dyn Receiver>),
                             || {}, |_| {});
        feed.start();
        *self.0.feed.lock().unwrap() = Some(feed);
    }

    fn end(&self) {
        if let Some(feed) = self.0.feed.lock().unwrap().take() {
            feed.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_read_as_pw_link_says_them() {
        let mut parser = LinkParser::default();
        assert_eq!(parser.parse("=  Chrome:output_FL"), None);
        assert_eq!(parser.parse("=   |-> plusone_speakers:playback_FL"),
                   Some(('=', ("Chrome:output_FL".into(), "plusone_speakers:playback_FL".into()))));
        assert_eq!(parser.parse("+ Chrome:output_FR -> plusone_speakers:playback_FR"),
                   Some(('+', ("Chrome:output_FR".into(), "plusone_speakers:playback_FR".into()))));
        assert_eq!(parser.parse("- Chrome:output_FR -> plusone_speakers:playback_FR").unwrap().0, '-');
        assert_eq!(parser.parse("something else"), None);
    }

    #[test]
    fn who_plays_into_the_sink() {
        let dump = r#"[
            {"id": 30, "type": "PipeWire:Interface:Node", "info": {"props": {"node.name": "plusone_speakers"}}},
            {"id": 40, "type": "PipeWire:Interface:Node", "info": {"props": {"application.name": "Chrome"}}},
            {"id": 41, "type": "PipeWire:Interface:Node", "info": {"props": {"node.name": "mpv"}}},
            {"id": 50, "type": "PipeWire:Interface:Link", "info": {"output-node-id": 40, "input-node-id": 30}},
            {"id": 51, "type": "PipeWire:Interface:Link", "info": {"output-node-id": 41, "input-node-id": 30}}
        ]
        [{"id": 51, "type": "PipeWire:Interface:Link", "info": null}]"#;
        assert_eq!(players(&pw_objects(dump), "plusone_speakers"), vec!["Chrome".to_owned()]);
    }
}
