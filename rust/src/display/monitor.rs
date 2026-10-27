//! A virtual monitor and remote input through Mutter's private D-Bus APIs
//! (display.py). Unlike the XDG portal they need no consent dialog, so the
//! monitor appears as soon as the tablet connects; it takes the size asked
//! for on the PipeWire stream and goes when the session stops.
//!
//! Input goes as D-Bus messages that expect no reply: nothing waits on
//! Mutter, the next finger's position is on its way at once.

use crate::log;
use glib::variant::{ObjectPath, ToVariant};
use glib::Variant;
use std::collections::HashMap;
use std::sync::Mutex;

const RD_NAME: &str = "org.gnome.Mutter.RemoteDesktop";
const SC_NAME: &str = "org.gnome.Mutter.ScreenCast";
const DISPLAY_CONFIG: &str = "org.gnome.Mutter.DisplayConfig";
const DISPLAY_CONFIG_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
// NotifyPointerAxis' flags (Mutter's MetaRemoteDesktopNotifyAxisFlags).
const AXIS_FINISH: u32 = 1 << 0;
const AXIS_SOURCE_FINGER: u32 = 1 << 2;

/// Where the desktop's input goes: the virtual monitor's session (or, in
/// tests, a recording).
pub trait Injector: Send + Sync {
    fn touch_down(&self, slot: u32, x: f64, y: f64);
    fn touch_motion(&self, slot: u32, x: f64, y: f64);
    fn touch_up(&self, slot: u32);
    fn pointer_move(&self, x: f64, y: f64);
    fn pointer_button(&self, button: i32, pressed: bool);
    /// Scrolling as from a touchpad's fingers, in desktop pixels; finish
    /// when they lifted (a program may then go on by itself).
    fn pointer_axis(&self, dx: f64, dy: f64, finish: bool);
    fn keysym(&self, keysym: u32, pressed: bool);
}

struct Session {
    rd: Option<String>,
    stream: String,
    subscriptions: Vec<gio::SignalSubscriptionId>,
}

pub struct VirtualMonitor {
    bus: gio::DBusConnection,
    session: Mutex<Session>,
}

fn call(bus: &gio::DBusConnection, name: &str, path: &str, iface: &str, method: &str,
        args: Option<&Variant>, reply: Option<&str>) -> Result<Variant, glib::Error> {
    bus.call_sync(Some(name), path, iface, method, args, reply.map(|r| glib::VariantTy::new(r).unwrap()),
                  gio::DBusCallFlags::NONE, -1, gio::Cancellable::NONE)
}

fn object_path(reply: &Variant) -> String {
    reply.child_value(0).str().unwrap_or_default().to_owned()
}

impl VirtualMonitor {
    /// on_ready(node): on the main loop, once PipeWire has the stream;
    /// on_closed(): if the session ends without stop() (GNOME's screen
    /// sharing indicator stopped it, say). Call on the main loop.
    pub fn start(on_ready: impl Fn(u32) + 'static, on_closed: impl Fn() + 'static) -> Result<VirtualMonitor, glib::Error> {
        let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?;
        let rd = object_path(&call(&bus, RD_NAME, "/org/gnome/Mutter/RemoteDesktop", RD_NAME, "CreateSession", None, Some("(o)"))?);
        let id = call(&bus, RD_NAME, &rd, "org.freedesktop.DBus.Properties", "Get",
                      Some(&(format!("{RD_NAME}.Session"), "SessionId").to_variant()), Some("(v)"))?
            .child_value(0).as_variant().and_then(|v| v.str().map(str::to_owned)).unwrap_or_default();
        let options = HashMap::from([("remote-desktop-session-id", id.to_variant())]);
        let sc = object_path(&call(&bus, SC_NAME, "/org/gnome/Mutter/ScreenCast", SC_NAME, "CreateSession",
                                   Some(&(options,).to_variant()), Some("(o)"))?);
        // cursor-mode 1 = embedded: the tablet has no cursor of its own.
        let options = HashMap::from([("cursor-mode", 1u32.to_variant()), ("is-platform", true.to_variant())]);
        let stream = object_path(&call(&bus, SC_NAME, &sc, &format!("{SC_NAME}.Session"), "RecordVirtual",
                                       Some(&(options,).to_variant()), Some("(o)"))?);
        #[allow(deprecated)]
        let added = bus.signal_subscribe(Some(SC_NAME), Some(&format!("{SC_NAME}.Stream")), Some("PipeWireStreamAdded"),
            Some(&stream), None, gio::DBusSignalFlags::NONE, move |_, _, _, _, _, params| {
                if let Some((node,)) = params.get::<(u32,)>() {
                    log!("virtual monitor PipeWire node {node}");
                    on_ready(node);
                }
            });
        #[allow(deprecated)]
        let closed = bus.signal_subscribe(Some(RD_NAME), Some(&format!("{RD_NAME}.Session")), Some("Closed"),
            Some(&rd), None, gio::DBusSignalFlags::NONE, move |_, _, _, _, _, _| {
                log!("virtual monitor session closed by GNOME");
                on_closed();
            });
        call(&bus, RD_NAME, &rd, &format!("{RD_NAME}.Session"), "Start", None, None)?;
        log!("virtual monitor session started");
        Ok(VirtualMonitor { bus, session: Mutex::new(Session { rd: Some(rd), stream, subscriptions: vec![added, closed] }) })
    }

    pub fn stop(&self) {
        let mut session = self.session.lock().unwrap();
        #[allow(deprecated)]
        for subscription in session.subscriptions.drain(..) {
            self.bus.signal_unsubscribe(subscription);
        }
        if let Some(rd) = session.rd.take() {
            if let Err(e) = call(&self.bus, RD_NAME, &rd, &format!("{RD_NAME}.Session"), "Stop", None, None) {
                log!("stopping remote desktop session: {}", e.message());
            }
        }
    }

    /// One input event to the session, no reply awaited.
    fn notify(&self, method: &str, args: Variant) {
        let session = self.session.lock().unwrap();
        let Some(rd) = &session.rd else { return };
        let message = gio::DBusMessage::new_method_call(Some(RD_NAME), rd, Some(&format!("{RD_NAME}.Session")), method);
        message.set_body(&args);
        message.set_flags(gio::DBusMessageFlags::NO_REPLY_EXPECTED);
        let _ = self.bus.send_message(&message, gio::DBusSendMessageFlags::NONE);
    }

    fn stream(&self) -> String {
        self.session.lock().unwrap().stream.clone()
    }
}

impl Injector for VirtualMonitor {
    fn touch_down(&self, slot: u32, x: f64, y: f64) {
        self.notify("NotifyTouchDown", (ObjectPath::try_from(self.stream()).unwrap(), slot, x, y).to_variant());
    }

    fn touch_motion(&self, slot: u32, x: f64, y: f64) {
        self.notify("NotifyTouchMotion", (ObjectPath::try_from(self.stream()).unwrap(), slot, x, y).to_variant());
    }

    fn touch_up(&self, slot: u32) {
        self.notify("NotifyTouchUp", (slot,).to_variant());
    }

    fn pointer_move(&self, x: f64, y: f64) {
        self.notify("NotifyPointerMotionAbsolute", (ObjectPath::try_from(self.stream()).unwrap(), x, y).to_variant());
    }

    fn pointer_button(&self, button: i32, pressed: bool) {
        self.notify("NotifyPointerButton", (button, pressed).to_variant());
    }

    fn pointer_axis(&self, dx: f64, dy: f64, finish: bool) {
        let flags = AXIS_SOURCE_FINGER | if finish { AXIS_FINISH } else { 0 };
        self.notify("NotifyPointerAxis", (dx, dy, flags).to_variant());
    }

    fn keysym(&self, keysym: u32, pressed: bool) {
        self.notify("NotifyKeyboardKeysym", (keysym, pressed).to_variant());
    }
}

/// Mutter's monitors: [(connector, vendor, product, serial), its modes
/// [(id, scales, current)]] and the logical monitors as GetCurrentState says.
fn current_state(bus: &gio::DBusConnection) -> Result<Variant, glib::Error> {
    call(bus, DISPLAY_CONFIG, DISPLAY_CONFIG_PATH, DISPLAY_CONFIG, "GetCurrentState", None, None)
}

fn spec(monitor: &Variant) -> (String, String, String, String) {
    monitor.child_value(0).get::<(String, String, String, String)>().unwrap_or_default()
}

/// (vendor, product, serial) of the newest Mutter virtual monitor.
pub fn virtual_monitor_edid() -> Option<(String, String, String)> {
    let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok()?;
    let state = current_state(&bus).ok()?;
    let monitors = state.child_value(1);
    (0..monitors.n_children()).map(|i| spec(&monitors.child_value(i)))
        .filter(|s| s.0.starts_with("Meta-"))
        .max_by_key(|s| u64::from_str_radix(&s.3, 16).unwrap_or(0))
        .map(|(_, vendor, product, serial)| (vendor, product, serial))
}

/// The newest virtual monitor at this scale (2: HiDPI), the others as they
/// are, through the configuration call GNOME Settings uses, temporarily:
/// whether it could. Mutter forgets a virtual monitor's settings (its serial
/// is new every session), so it is set each time.
pub fn set_virtual_monitor_scale(scale: f64) -> Result<bool, glib::Error> {
    let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?;
    let state = current_state(&bus)?;
    let serial = state.child_value(0).get::<u32>().unwrap_or(0);
    let monitors = state.child_value(1);
    let logical = state.child_value(2);
    let mut current: HashMap<String, (String, Vec<f64>)> = HashMap::new();  // connector -> (current mode, its scales)
    let mut virtual_monitors = Vec::new();
    for i in 0..monitors.n_children() {
        let monitor = monitors.child_value(i);
        let spec = spec(&monitor);
        let modes = monitor.child_value(1);
        for j in 0..modes.n_children() {
            let mode = modes.child_value(j);
            let props = mode.child_value(6);
            let is_current = glib::VariantDict::new(Some(&props)).lookup_value("is-current", None)
                .and_then(|v| v.get::<bool>()).unwrap_or(false);
            if is_current {
                let id = mode.child_value(0).get::<String>().unwrap_or_default();
                let scales = mode.child_value(5).get::<Vec<f64>>().unwrap_or_default();
                current.insert(spec.0.clone(), (id, scales));
            }
        }
        if spec.0.starts_with("Meta-") {
            virtual_monitors.push(spec);
        }
    }
    let Some(target) = virtual_monitors.iter().max_by_key(|s| u64::from_str_radix(&s.3, 16).unwrap_or(0)).map(|s| s.0.clone())
    else { return Ok(false) };
    if !current.get(&target).is_some_and(|(_, scales)| scales.iter().any(|s| (s - scale).abs() < 0.01)) {
        log!("display: scale {scale} not offered for the tablet's monitor");
        return Ok(false);
    }
    type Logical = (i32, i32, f64, u32, bool, Vec<(String, String, HashMap<String, Variant>)>);
    let mut config: Vec<Logical> = Vec::new();
    for i in 0..logical.n_children() {
        let l = logical.child_value(i);
        let (x, y) = (l.child_value(0).get::<i32>().unwrap_or(0), l.child_value(1).get::<i32>().unwrap_or(0));
        let lscale = l.child_value(2).get::<f64>().unwrap_or(1.0);
        let transform = l.child_value(3).get::<u32>().unwrap_or(0);
        let primary = l.child_value(4).get::<bool>().unwrap_or(false);
        let specs = l.child_value(5);
        let connectors: Vec<String> = (0..specs.n_children())
            .map(|j| specs.child_value(j).child_value(0).get::<String>().unwrap_or_default()).collect();
        if connectors.iter().any(|c| !current.contains_key(c)) {
            return Ok(false);  // a monitor without a current mode: not ours to guess
        }
        let monitors = connectors.iter().map(|c| (c.clone(), current[c].0.clone(), HashMap::new())).collect();
        config.push((x, y, if connectors.contains(&target) { scale } else { lscale }, transform, primary, monitors));
    }
    call(&bus, DISPLAY_CONFIG, DISPLAY_CONFIG_PATH, DISPLAY_CONFIG, "ApplyMonitorsConfig",
         Some(&(serial, 1u32, config, HashMap::<String, Variant>::new()).to_variant()), None)?;
    log!("display: the tablet's monitor at scale {scale}");
    Ok(true)
}
