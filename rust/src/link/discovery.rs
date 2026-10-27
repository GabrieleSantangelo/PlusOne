//! Discovery: where the tablets' apps are (discovery.py, nearby.py).
//!
//! Nearby watches Avahi (mDNS) for Plus One's own app (_plusone._tcp: where
//! to connect, its id and version) and for others that say a tablet is
//! there at all (Quick Share, KDE Connect). Avahi listens to the network
//! anyway, so watching costs next to nothing. Each service is resolved
//! again every RECHECK: a tablet that leaves without saying goodbye fails
//! to resolve once its SRV record is gone.
//!
//! Nearby lives on the main loop; a View of it (Send) answers other threads.

use super::link::Target;
use crate::log;
use glib::variant::ToVariant;
use glib::Variant;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const AVAHI: &str = "org.freedesktop.Avahi";
const SERVER: &str = "org.freedesktop.Avahi.Server";
const BROWSER: &str = "org.freedesktop.Avahi.ServiceBrowser";
const QUICK_SHARE: &str = "_FC9F5ED42C8A._tcp";
const KDE_CONNECT: &str = "_kdeconnect._udp";
pub const COMPANION: &str = "_plusone._tcp";
const SERVICES: [&str; 3] = [QUICK_SHARE, KDE_CONNECT, COMPANION];
const UNSPEC: i32 = -1;  // any interface, any protocol
const INET: i32 = 0;     // Avahi's IPv4: the app is reached at an IPv4 address
const TABLET: u8 = 2;    // Quick Share device type: 1 phone, 2 tablet, 3 laptop
const RECHECK: u32 = 60;  // seconds
const RETRIES: [u32; 3] = [2, 5, 15];  // seconds
const RESOLVE_TIMEOUT: i32 = 10_000;  // ms; Avahi gives up after 5 s

/// (interface, protocol, name, type, domain) of an announced service.
type Key = (i32, i32, String, String, String);

/// The app on a tablet: (app id, address, port, name).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct App {
    pub id: String,
    pub address: String,
    pub port: u16,
    pub name: String,
}

#[derive(Default)]
struct Seen {
    known: bool,                           // Avahi is there to ask
    tablets: BTreeMap<Key, String>,        // services on a tablet -> its name ("" unknown)
    companions: BTreeMap<Key, App>,        // the app's services
    versions: BTreeMap<Key, u32>,          // their versionCode ("v"; 0 before 0.8)
}

/// What a way of finding the apps says (discovery.py's sources).
pub trait Source: Send + Sync {
    fn present(&self) -> bool;
    fn names(&self) -> Vec<String>;
    fn apps(&self) -> Vec<App>;
    fn app_version(&self, app_id: &str) -> Option<u32>;
    fn addresses(&self, app_id: &str) -> Vec<Target>;
    fn refresh(&self);
}

/// Nearby's answers, from any thread.
#[derive(Clone)]
pub struct View {
    seen: Arc<Mutex<Seen>>,
    refresh: Arc<AtomicBool>,
}

impl Source for View {
    /// A tablet is on the network, or there is no telling.
    fn present(&self) -> bool {
        let seen = self.seen.lock().unwrap();
        !seen.tablets.is_empty() || !seen.known
    }

    fn names(&self) -> Vec<String> {
        let seen = self.seen.lock().unwrap();
        seen.tablets.values().filter(|n| !n.is_empty()).cloned().collect::<BTreeSet<_>>().into_iter().collect()
    }

    /// One per tablet, which may show up on two interfaces; by name.
    fn apps(&self) -> Vec<App> {
        let seen = self.seen.lock().unwrap();
        let mut by_id: BTreeMap<String, App> = BTreeMap::new();
        let mut all: Vec<&App> = seen.companions.values().collect();
        all.sort_by(|a, b| (&a.address, a.port, &a.name, &a.id).cmp(&(&b.address, b.port, &b.name, &b.id)));
        for app in all {
            let key = if app.id.is_empty() { format!("{}:{}", app.address, app.port) } else { app.id.clone() };
            by_id.entry(key).or_insert_with(|| app.clone());
        }
        let mut apps: Vec<App> = by_id.into_values().collect();
        apps.sort_by(|a, b| (a.name.to_lowercase(), &a.id).cmp(&(b.name.to_lowercase(), &b.id)));
        apps
    }

    fn app_version(&self, app_id: &str) -> Option<u32> {
        let seen = self.seen.lock().unwrap();
        seen.companions.iter().filter(|(_, a)| a.id == app_id)
            .map(|(k, _)| seen.versions.get(k).copied().unwrap_or(0)).max()
    }

    /// Every (address, port) the app with this id is announced at: someone
    /// else may announce it too, and only the real one proves it (TLS).
    fn addresses(&self, app_id: &str) -> Vec<Target> {
        let seen = self.seen.lock().unwrap();
        seen.companions.values().filter(|a| a.id == app_id)
            .map(|a| Target::Tcp(a.address.clone(), a.port)).collect::<BTreeSet<_>>().into_iter().collect()
    }

    /// Asked where the apps are now (the user wants a tablet).
    fn refresh(&self) {
        self.refresh.store(true, Ordering::Relaxed);
    }
}

struct Inner {
    bus: Option<gio::DBusConnection>,
    services: BTreeSet<Key>,
    failed: HashMap<Key, usize>,
    browsers: Vec<String>,
    subscription: Option<gio::SignalSubscriptionId>,
    unwatch: Option<Box<dyn FnOnce()>>,
    timers: Vec<glib::SourceId>,
}

/// Watches Avahi on the main loop; on_change() when what it sees may have changed.
#[derive(Clone)]
pub struct Nearby {
    inner: Rc<RefCell<Inner>>,
    view: View,
    on_change: Rc<dyn Fn()>,
}

impl Nearby {
    pub fn new(on_change: impl Fn() + 'static) -> Nearby {
        Nearby {
            inner: Rc::new(RefCell::new(Inner {
                bus: None, services: BTreeSet::new(), failed: HashMap::new(), browsers: Vec::new(),
                subscription: None, unwatch: None, timers: Vec::new(),
            })),
            view: View { seen: Arc::default(), refresh: Arc::default() },
            on_change: Rc::new(on_change),
        }
    }

    pub fn view(&self) -> View {
        self.view.clone()
    }

    pub fn start(&self) {
        if self.inner.borrow().bus.is_some() {
            return;
        }
        let bus = match gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE) {
            Ok(bus) => bus,
            Err(e) => return log!("nearby: no system bus: {}", e.message()),
        };
        // All browsers' signals, filtered by service type: ours may send some
        // before we learn their paths.
        let me = self.clone();
        #[allow(deprecated)]
        let subscription = bus.signal_subscribe(None, Some(BROWSER), None, None, None, gio::DBusSignalFlags::NONE,
            move |_, _, _, _, signal, params| me.signal(signal, params));
        let (appeared, vanished) = (self.clone(), self.clone());
        let watch = gio::bus_watch_name_on_connection(&bus, AVAHI, gio::BusNameWatcherFlags::NONE,
            move |bus, _, _| appeared.appeared(bus), move |_, _| vanished.vanished());
        let me = self.clone();
        let recheck = glib::timeout_add_seconds_local(RECHECK, move || {
            me.recheck();
            glib::ControlFlow::Continue
        });
        let me = self.clone();
        let asked = glib::timeout_add_local(Duration::from_millis(500), move || {
            if me.view.refresh.swap(false, Ordering::Relaxed) {
                me.resolve_companions();
            }
            glib::ControlFlow::Continue
        });
        let mut inner = self.inner.borrow_mut();
        inner.subscription = Some(subscription);
        inner.unwatch = Some(Box::new(move || gio::bus_unwatch_name(watch)));
        inner.timers = vec![recheck, asked];
        inner.bus = Some(bus);
    }

    pub fn stop(&self) {
        let mut inner = self.inner.borrow_mut();
        let Some(bus) = inner.bus.take() else { return };
        for path in inner.browsers.drain(..) {
            bus.call(Some(AVAHI), &path, BROWSER, "Free", None, None, gio::DBusCallFlags::NONE, -1,
                     gio::Cancellable::NONE, |_| {});
        }
        #[allow(deprecated)]
        if let Some(subscription) = inner.subscription.take() {
            bus.signal_unsubscribe(subscription);
        }
        if let Some(unwatch) = inner.unwatch.take() {
            unwatch();
        }
        for timer in inner.timers.drain(..) {
            timer.remove();
        }
        inner.services.clear();
        drop(inner);
        *self.view.seen.lock().unwrap() = Seen::default();
    }

    fn appeared(&self, bus: gio::DBusConnection) {
        self.view.seen.lock().unwrap().known = true;
        for service in SERVICES {
            let me = self.clone();
            bus.call(Some(AVAHI), "/", SERVER, "ServiceBrowserNew",
                     Some(&(UNSPEC, UNSPEC, service, "", 0u32).to_variant()), Some(glib::VariantTy::new("(o)").unwrap()),
                     gio::DBusCallFlags::NONE, -1, gio::Cancellable::NONE, move |result| match result {
                         Ok(reply) => {
                             if let Some((path,)) = reply.get::<(glib::variant::ObjectPath,)>() {
                                 me.inner.borrow_mut().browsers.push(path.as_str().to_owned());
                             }
                         }
                         Err(e) => log!("nearby: cannot browse: {}", e.message()),
                     });
        }
        (self.on_change)();
    }

    fn vanished(&self) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.browsers.clear();
            inner.services.clear();
        }
        *self.view.seen.lock().unwrap() = Seen::default();  // known: false
        (self.on_change)();
    }

    fn signal(&self, signal: &str, params: &Variant) {
        if signal != "ItemNew" && signal != "ItemRemove" {
            return;
        }
        let Some((interface, protocol, name, service, domain, _flags)) =
            params.get::<(i32, i32, String, String, String, u32)>() else { return };
        if !SERVICES.contains(&service.as_str()) {
            return;
        }
        let key: Key = (interface, protocol, name, service, domain);
        if signal == "ItemNew" {
            if self.inner.borrow_mut().services.insert(key.clone()) {
                self.resolve(key);
            }
        } else {
            self.inner.borrow_mut().services.remove(&key);
            let gone = self.view.seen.lock().unwrap().companions.remove(&key).is_some();
            if gone {
                log!("nearby: the Plus One app left the network");
                (self.on_change)();
            }
            self.update(&key, None);
        }
    }

    fn resolve(&self, key: Key) {
        let Some(bus) = self.inner.borrow().bus.clone() else { return };
        let aprotocol = if key.3 == COMPANION { INET } else { UNSPEC };
        let params = (key.0, key.1, key.2.as_str(), key.3.as_str(), key.4.as_str(), aprotocol, 0u32).to_variant();
        let me = self.clone();
        bus.call(Some(AVAHI), "/", SERVER, "ResolveService", Some(&params),
                 Some(glib::VariantTy::new("(iissssisqaayu)").unwrap()), gio::DBusCallFlags::NONE,
                 RESOLVE_TIMEOUT, gio::Cancellable::NONE, move |result| me.resolved(key, result.ok()));
    }

    fn resolved(&self, key: Key, reply: Option<Variant>) {
        type Reply = (i32, i32, String, String, String, String, i32, String, u16, Vec<Vec<u8>>, u32);
        let reply = reply.and_then(|r| r.get::<Reply>());  // None: gone, or not answering any more
        if !self.inner.borrow().services.contains(&key) {
            return;
        }
        if key.3 == COMPANION {
            let failed = if reply.is_some() { 0 } else { self.inner.borrow().failed.get(&key).copied().unwrap_or(0) + 1 };
            self.inner.borrow_mut().failed.insert(key.clone(), failed);
            if failed > 0 && failed <= RETRIES.len() {
                let me = self.clone();
                let again = key.clone();
                glib::timeout_add_seconds_local_once(RETRIES[failed - 1], move || {
                    if me.inner.borrow().bus.is_some() && me.inner.borrow().services.contains(&again) {
                        me.resolve(again);
                    }
                });
                if self.view.seen.lock().unwrap().companions.contains_key(&key) {
                    return;  // one answer lost is not the app gone (the tablet's speakers with it)
                }
            }
            self.companion(&key, reply.as_ref().map(|r| (r.7.clone(), r.8, &r.9)));
            // The app runs on a tablet: that is a tablet on the network too.
            let name = self.view.seen.lock().unwrap().companions.get(&key).map(|a| a.name.clone());
            self.update(&key, name);
            return;
        }
        let name = reply.and_then(|r| tablet_name(&key.3, &r.9));
        self.update(&key, name);
    }

    fn companion(&self, key: &Key, reply: Option<(String, u16, &Vec<Vec<u8>>)>) {
        let changed;
        let said;
        {
            let mut seen = self.view.seen.lock().unwrap();
            let before = (seen.companions.get(key).cloned(), seen.versions.get(key).copied());
            match reply {
                Some((address, port, txt)) => {
                    let fields = txt_fields(txt);
                    let text = |k: &str| String::from_utf8_lossy(fields.get(k).map(Vec::as_slice).unwrap_or(&[])).into_owned();
                    let name: String = Some(text("name")).filter(|n| !n.is_empty()).unwrap_or_else(|| key.2.clone())
                        .chars().take(64).collect();
                    let id: String = text("id").to_lowercase().chars().take(64).collect();
                    let version = text("v");
                    seen.companions.insert(key.clone(), App { id, address, port, name });
                    let v = if !version.is_empty() && version.len() <= 9 { version.parse().unwrap_or(0) } else { 0 };
                    seen.versions.insert(key.clone(), v);
                }
                None => {
                    seen.companions.remove(key);
                    seen.versions.remove(key);
                }
            }
            changed = (seen.companions.get(key).cloned(), seen.versions.get(key).copied()) != before;
            said = seen.companions.get(key).map(|a| format!("the Plus One app on {} at {}:{}", a.name, a.address, a.port));
        }
        if changed {
            log!("nearby: {}", said.unwrap_or_else(|| "the Plus One app is gone".into()));
            (self.on_change)();
        }
    }

    /// Each tablet shows up once per app and IP version: changes only.
    fn update(&self, key: &Key, name: Option<String>) {
        let (before, after);
        {
            let view = &self.view;
            before = (view.present_tablets(), view.names());
            let mut seen = view.seen.lock().unwrap();
            match name {
                Some(name) => {
                    seen.tablets.insert(key.clone(), name);
                }
                None => {
                    seen.tablets.remove(key);
                }
            }
            drop(seen);
            after = (view.present_tablets(), view.names());
        }
        if after != before {
            if after.0 {
                let names = if after.1.is_empty() { "unnamed".to_owned() } else { after.1.join(", ") };
                log!("nearby: tablet on the network ({names})");
            } else {
                log!("nearby: no tablet on the network");
            }
            (self.on_change)();
        }
    }

    fn resolve_companions(&self) {
        let keys: Vec<Key> = self.inner.borrow().services.iter().filter(|k| k.3 == COMPANION).cloned().collect();
        for key in keys {
            self.resolve(key);
        }
    }

    fn recheck(&self) {
        let keys: Vec<Key> = self.inner.borrow().services.iter().cloned().collect();
        for key in keys {
            self.resolve(key);
        }
    }
}

impl View {
    fn present_tablets(&self) -> bool {
        !self.seen.lock().unwrap().tablets.is_empty()
    }
}

fn txt_fields(txt: &[Vec<u8>]) -> HashMap<String, Vec<u8>> {
    txt.iter().map(|entry| {
        let at = entry.iter().position(|&b| b == b'=').unwrap_or(entry.len());
        (String::from_utf8_lossy(&entry[..at]).into_owned(), entry.get(at + 1..).unwrap_or(&[]).to_vec())
    }).collect()
}

/// For a service's TXT record: the device's name if it is a tablet ("" if
/// it does not say), None if it is not one.
pub fn tablet_name(service: &str, txt: &[Vec<u8>]) -> Option<String> {
    let fields = txt_fields(txt);
    if service == KDE_CONNECT {
        if fields.get("type").map(Vec::as_slice) != Some(b"tablet") {
            return None;
        }
        return Some(String::from_utf8_lossy(fields.get("name").map(Vec::as_slice).unwrap_or(&[])).into_owned());
    }
    // Quick Share: n is the endpoint info, base64url. Its first byte holds the
    // device type in bits 1-3; when visible to everyone, a length-prefixed
    // name follows 16 bytes of salt and key.
    let info = base64url(fields.get("n").map(Vec::as_slice).unwrap_or(&[]))?;
    if info.is_empty() || (info[0] >> 1) & 7 != TABLET {
        return None;
    }
    if info.len() > 18 && info.len() >= 18 + info[17] as usize {
        return Some(String::from_utf8_lossy(&info[18..18 + info[17] as usize]).into_owned());
    }
    Some(String::new())
}

/// base64url, its padding optional; None if it is not.
fn base64url(text: &[u8]) -> Option<Vec<u8>> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    };
    let text: Vec<u8> = text.iter().copied().take_while(|&c| c != b'=').collect();
    if text.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    for chunk in text.chunks(4) {
        let v: Vec<u8> = chunk.iter().map(|&c| value(c)).collect::<Option<_>>()?;
        let n = v.iter().enumerate().fold(0u32, |n, (i, &x)| n | (x as u32) << (18 - 6 * i));
        out.extend_from_slice(&n.to_be_bytes()[1..chunk.len()]);
    }
    Some(out)
}

/// Every way of finding the tablets' apps, as one: a tablet is there if any
/// source sees it, its app is reached wherever any of them says.
#[derive(Clone)]
pub struct Discovery {
    sources: Vec<Arc<dyn Source>>,
}

impl Discovery {
    pub fn new(sources: Vec<Arc<dyn Source>>) -> Discovery {
        Discovery { sources }
    }
}

impl Source for Discovery {
    fn present(&self) -> bool {
        self.sources.iter().any(|s| s.present())
    }

    fn names(&self) -> Vec<String> {
        self.sources.iter().flat_map(|s| s.names()).collect::<BTreeSet<_>>().into_iter().collect()
    }

    fn apps(&self) -> Vec<App> {
        let mut seen: BTreeMap<String, App> = BTreeMap::new();
        for app in self.sources.iter().flat_map(|s| s.apps()) {
            let key = if app.id.is_empty() { format!("{}:{}", app.address, app.port) } else { app.id.clone() };
            seen.entry(key).or_insert(app);
        }
        let mut apps: Vec<App> = seen.into_values().collect();
        apps.sort_by(|a, b| (a.name.to_lowercase(), &a.id).cmp(&(b.name.to_lowercase(), &b.id)));
        apps
    }

    fn app_version(&self, app_id: &str) -> Option<u32> {
        self.sources.iter().filter_map(|s| s.app_version(app_id)).max()
    }

    fn addresses(&self, app_id: &str) -> Vec<Target> {
        let mut out = Vec::new();
        for target in self.sources.iter().flat_map(|s| s.addresses(app_id)) {
            if !out.contains(&target) {
                out.push(target);
            }
        }
        out
    }

    fn refresh(&self) {
        for source in &self.sources {
            source.refresh();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_share_and_kde_connect_say_which_is_a_tablet() {
        // Quick Share's endpoint info: type 2 (tablet) in bits 1-3, 16 bytes, the name's length, the name.
        let mut info = vec![TABLET << 1];
        info.extend_from_slice(&[0; 16]);
        info.push(3);
        info.extend_from_slice(b"Tab");
        let n = encode64url(&info);
        assert_eq!(tablet_name(QUICK_SHARE, &[[b"n=".as_slice(), n.as_bytes()].concat()]), Some("Tab".into()));
        let phone = encode64url(&[1 << 1]);
        assert_eq!(tablet_name(QUICK_SHARE, &[[b"n=".as_slice(), phone.as_bytes()].concat()]), None);
        assert_eq!(tablet_name(KDE_CONNECT, &[b"type=tablet".to_vec(), b"name=Tab".to_vec()]), Some("Tab".into()));
        assert_eq!(tablet_name(KDE_CONNECT, &[b"type=phone".to_vec()]), None);
    }

    fn encode64url(data: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
            for i in 0..chunk.len() + 1 {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
        }
        out
    }

    struct Fixed(Vec<App>, bool);

    impl Source for Fixed {
        fn present(&self) -> bool { self.1 }
        fn names(&self) -> Vec<String> { self.0.iter().map(|a| a.name.clone()).collect() }
        fn apps(&self) -> Vec<App> { self.0.clone() }
        fn app_version(&self, id: &str) -> Option<u32> { self.0.iter().any(|a| a.id == id).then_some(800) }
        fn addresses(&self, id: &str) -> Vec<Target> {
            self.0.iter().filter(|a| a.id == id).map(|a| Target::Tcp(a.address.clone(), a.port)).collect()
        }
        fn refresh(&self) {}
    }

    #[test]
    fn sources_answer_as_one() {
        let app = |id: &str, address: &str, name: &str| App { id: id.into(), address: address.into(), port: 47236, name: name.into() };
        let found = Discovery::new(vec![
            Arc::new(Fixed(vec![app("aaaa", "10.0.0.2", "Kitchen")], true)),
            Arc::new(Fixed(vec![app("aaaa", "10.0.0.9", "Kitchen"), app("bbbb", "10.0.0.3", "Office")], false)),
        ]);
        assert!(found.present());
        assert_eq!(found.addresses("aaaa"), vec![Target::Tcp("10.0.0.2".into(), 47236), Target::Tcp("10.0.0.9".into(), 47236)]);
        assert_eq!(found.apps().iter().map(|a| a.address.as_str()).collect::<Vec<_>>(), vec!["10.0.0.2", "10.0.0.3"]);
        assert_eq!(found.app_version("cccc"), None);
    }
}
