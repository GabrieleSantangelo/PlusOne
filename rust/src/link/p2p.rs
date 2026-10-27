//! Wi-Fi Direct with the tablet's app through NetworkManager: a path of
//! their own, each packet once through the air (not twice, through an
//! access point or a phone's hotspot).
//!
//! The PC looks for peers (and so can be seen), asks the app over the link
//! it has (P2pHandler.kt) with the name it has on Wi-Fi Direct; the app
//! connects to it as the group owner and answers with its P2P address; the
//! PC connects to that peer at once. Both sides asked: neither shows a
//! question. The connection is NetworkManager's, volatile and bound to this
//! process: it goes when the process does. As learned with Second Screen
//! (git history, plusone/p2p.py): an activation is never cut short.

use crate::log;
use glib::variant::ToVariant;
use glib::Variant;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const P2P: &str = "org.freedesktop.NetworkManager.Device.WifiP2P";
const PEER: &str = "org.freedesktop.NetworkManager.WifiP2PPeer";
const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const DEVICE_TYPE_WIFI_P2P: u32 = 30;
const ACTIVATED: u32 = 2;
const DEACTIVATED: u32 = 4;
pub const ACTIVATION: Duration = Duration::from_secs(55);  // wpa_supplicant repeats its request this long
pub const GROUP_OWNER: &str = "192.168.49.1";  // Android's group owner, always

fn call(bus: &gio::DBusConnection, path: &str, iface: &str, method: &str, args: Option<&Variant>) -> Result<Variant, String> {
    bus.call_sync(Some(NM), path, iface, method, args, None, gio::DBusCallFlags::NONE, 60_000, gio::Cancellable::NONE)
        .map_err(|e| e.message().to_owned())
}

fn property(bus: &gio::DBusConnection, path: &str, iface: &str, name: &str) -> Option<Variant> {
    call(bus, path, "org.freedesktop.DBus.Properties", "Get", Some(&(iface, name).to_variant())).ok()?
        .child_value(0).as_variant()
}

fn paths(value: Option<Variant>) -> Vec<String> {
    value.map(|v| (0..v.n_children()).filter_map(|i| v.child_value(i).str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// This PC's P2P device address, as iw tells it (NetworkManager does not).
pub fn own_address() -> Option<String> {
    let out = std::process::Command::new("iw").arg("dev").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut address = None;
    for line in text.lines().map(str::trim) {
        if let Some(a) = line.strip_prefix("addr ") {
            address = Some(a.to_lowercase());
        } else if line == "type P2P-device" {
            return address;
        } else if line.starts_with("Interface") || line.starts_with("Unnamed") {
            address = None;
        }
    }
    None
}

/// A Wi-Fi Direct group being made, then up.
pub struct Group {
    bus: gio::DBusConnection,
    device: String,
    active: Option<String>,
}

impl Group {
    /// NetworkManager's Wi-Fi Direct device, looking for peers (seen by them too).
    pub fn find() -> Result<Group, String> {
        let bus = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE).map_err(|e| e.message().to_owned())?;
        let devices = paths(call(&bus, NM_PATH, NM, "GetDevices", None).ok().map(|v| v.child_value(0)));
        let device = devices.into_iter()
            .find(|d| property(&bus, d, "org.freedesktop.NetworkManager.Device", "DeviceType")
                .and_then(|v| v.get::<u32>()) == Some(DEVICE_TYPE_WIFI_P2P))
            .ok_or("no Wi-Fi Direct device")?;
        let options = HashMap::from([("timeout", 120i32.to_variant())]);
        call(&bus, &device, P2P, "StartFind", Some(&(options,).to_variant()))?;
        Ok(Group { bus, device, active: None })
    }

    /// The peer with this P2P address or name, once seen; (its path, its address).
    fn peer(&self, address: &str, name: &str, wait: Duration) -> Option<(String, String)> {
        let until = Instant::now() + wait;
        let mut seen = Vec::new();
        while Instant::now() < until {
            seen.clear();
            for peer in paths(property(&self.bus, &self.device, P2P, "Peers")) {
                let text = |p: &str| property(&self.bus, &peer, PEER, p).and_then(|v| v.str().map(str::to_owned)).unwrap_or_default();
                let (mac, peer_name) = (text("HwAddress").to_lowercase(), text("Name"));
                if mac == address || (!name.is_empty() && peer_name == name) {
                    return Some((peer, mac));
                }
                seen.push(format!("{peer_name} ({mac})"));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        log!("p2p: seen instead: {}", seen.join(", "));
        None
    }

    /// Starts connecting to the app (its P2P address, or its name when
    /// Android keeps the address from it); wait() for the group.
    pub fn join(&mut self, address: &str, name: &str) -> Result<(), String> {
        let t0 = Instant::now();
        let (peer, address) = self.peer(&address.to_lowercase(), name, Duration::from_secs(20))
            .ok_or("the tablet not seen on Wi-Fi Direct")?;
        log!("p2p: the tablet seen in {:.1} s", t0.elapsed().as_secs_f64());
        let user = glib::user_name().to_string_lossy().into_owned();
        let connection: HashMap<&str, HashMap<&str, Variant>> = HashMap::from([
            ("connection", HashMap::from([
                ("type", "wifi-p2p".to_variant()), ("id", "Plus One".to_variant()), ("autoconnect", false.to_variant()),
                ("permissions", vec![format!("user:{user}:")].to_variant())])),
            ("wifi-p2p", HashMap::from([("peer", address.to_variant())])),
            ("ipv4", HashMap::from([("method", "auto".to_variant()), ("never-default", true.to_variant())])),
            ("ipv6", HashMap::from([("method", "ignore".to_variant())])),
        ]);
        let options = HashMap::from([("persist", "volatile".to_variant()), ("bind-activation", "dbus-client".to_variant())]);
        let device = glib::variant::ObjectPath::try_from(self.device.clone()).unwrap();
        let peer_path = glib::variant::ObjectPath::try_from(peer).unwrap();
        let reply = call(&self.bus, NM_PATH, NM, "AddAndActivateConnection2",
                         Some(&(connection, device, peer_path, options).to_variant()))?;
        let active = reply.child_value(1).str().unwrap_or_default().to_owned();
        self.active = Some(active);
        Ok(())
    }

    /// Until the group is up, with an address: never cut short (the tablet
    /// may accept as late as the end).
    pub fn wait(&self) -> Result<(), String> {
        let t0 = Instant::now();
        let active = self.active.clone().ok_or("not joining")?;
        let until = Instant::now() + ACTIVATION;
        loop {
            match property(&self.bus, &active, ACTIVE, "State").and_then(|v| v.get::<u32>()) {
                Some(ACTIVATED) => break,
                Some(DEACTIVATED) | None => return Err("the group did not come up".into()),
                _ if Instant::now() > until => return Err("the group took too long".into()),
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        log!("p2p: the group is up in {:.1} s", t0.elapsed().as_secs_f64());
        Ok(())
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(active) = self.active.take() {
            let path = glib::variant::ObjectPath::try_from(active).unwrap();
            let _ = call(&self.bus, NM_PATH, NM, "DeactivateConnection", Some(&(path,).to_variant()));
        }
        let _ = call(&self.bus, &self.device, P2P, "StopFind", None);
    }
}
