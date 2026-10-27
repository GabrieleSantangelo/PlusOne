//! The S Pen from the tablet: a real Linux tablet device when possible
//! (pen.py). The app sends 10-byte reports of a digitizer pen (report ID
//! 0x14: tip/barrel/invert/eraser/in-range bits, X/Y 0..32767, pressure
//! 0..4096, X/Y tilt). Plus One's pen helper (root, data/pen-helper) makes a
//! virtual HID device with that descriptor and replays the reports, so the
//! kernel creates a proper tablet (pressure and tilt reach Krita, GIMP...);
//! /dev/uhid itself if this user may; else the pen drives the pointer.

use super::monitor::{virtual_monitor_edid, Injector};
use crate::log;
use gio::prelude::*;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const UHID_PATH: &str = "/dev/uhid";
const UHID_DESTROY: u32 = 1;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const BUS_VIRTUAL: u16 = 0x06;
const VENDOR: u32 = 0x04E8;   // Samsung
const PRODUCT: u32 = 0x5350;  // "SP"
const DEVICE_NAME: &str = "Plus One S Pen";
const TABLET_SCHEMA: &str = "org.gnome.desktop.peripherals.tablet";
pub const BTN_LEFT: i32 = 0x110;
pub const BTN_RIGHT: i32 = 0x111;
const TIP: u8 = 0x01;
const BARREL: u8 = 0x02;
pub const IN_RANGE: u8 = 0x10;
const AXIS_MAX: f64 = 32767.0;
pub const REPORT_ID: u8 = 0x14;
pub const REPORT_SIZE: usize = 10;
const HELPER: &str = "io.github.gabrielesantangelo.PlusOne.Pen";
const HELPER_PATH: &str = "/io/github/gabrielesantangelo/PlusOne/Pen";
/// The Galaxy Tab's S Pen as Second Screen described it (pen.py S_PEN).
const S_PEN: &str = "050d0902a10185140920a10009420944093c0945093215002501750195058102950381030501\
093075109501a4550d65133500463a2027ff7f000081020931462c1827ff7f00008102b4050d\
093026001081027508093d15a6255a8102093e8102c0c0";

fn descriptor() -> Vec<u8> {
    (0..S_PEN.len()).step_by(2).map(|i| u8::from_str_radix(&S_PEN[i..i + 2], 16).unwrap()).collect()
}

pub fn is_report(data: &[u8]) -> bool {
    data.len() == REPORT_SIZE && data[0] == REPORT_ID
}

pub trait Pen: Send {
    fn report(&mut self, data: &[u8]);
    fn close(&mut self);
}

/// The pen tablet the helper made: its reports go to it through the socket
/// (it checks each is one), and it goes when the socket closes.
struct HelperPen(UnixStream);

impl Pen for HelperPen {
    fn report(&mut self, data: &[u8]) {
        if is_report(data) {
            let _ = self.0.write(data);  // the helper behind: this one report goes, not the session
        }
    }

    fn close(&mut self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

/// The bus the helper is on (the system's, or the session's to try it out).
fn helper_bus() -> Option<gio::DBusConnection> {
    for bus_type in [gio::BusType::System, gio::BusType::Session] {
        let Ok(bus) = gio::bus_get_sync(bus_type, gio::Cancellable::NONE) else { continue };
        for method in ["ListNames", "ListActivatableNames"] {
            let names = bus.call_sync(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", "org.freedesktop.DBus",
                                      method, None, Some(glib::VariantTy::new("(as)").unwrap()),
                                      gio::DBusCallFlags::NONE, 2000, gio::Cancellable::NONE);
            if names.ok().and_then(|n| n.child_value(0).get::<Vec<String>>()).is_some_and(|n| n.iter().any(|x| x == HELPER)) {
                return Some(bus);
            }
        }
    }
    None
}

fn uhid_writable() -> bool {
    std::ffi::CString::new(UHID_PATH)
        .map(|p| unsafe { libc::access(p.as_ptr(), libc::R_OK | libc::W_OK) } == 0)
        .unwrap_or(false)
}

/// Whether the pen can be a tablet with pressure: the helper, or /dev/uhid.
pub fn uhid_available() -> bool {
    helper_bus().is_some() || uhid_writable()
}

fn helper_pen() -> Option<Box<dyn Pen>> {
    let bus = helper_bus()?;
    match bus.call_with_unix_fd_list_sync(Some(HELPER), HELPER_PATH, HELPER, "Open", None,
                                          Some(glib::VariantTy::new("(h)").unwrap()), gio::DBusCallFlags::NONE,
                                          10000, None::<&gio::UnixFDList>, gio::Cancellable::NONE) {
        Ok((reply, Some(fds))) => {
            let index = reply.child_value(0).get::<i32>()?;
            let raw = fds.get(index).ok()?;  // a duplicate, ours to close
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            let stream = UnixStream::from(fd);
            stream.set_nonblocking(true).ok()?;
            log!("pen: virtual HID tablet created by the helper (pressure and tilt)");
            Some(Box::new(HelperPen(stream)))
        }
        Ok(_) => None,
        Err(e) => {
            log!("pen: the helper said no: {}", e.message());
            None
        }
    }
}

/// /dev/uhid itself: struct uhid_event, packed, little-endian.
struct UhidPen {
    fd: Arc<Mutex<Option<OwnedFd>>>,  // closed by the drain thread, once told: its number never reused under a write
    closing: Arc<AtomicBool>,
}

impl UhidPen {
    fn open() -> std::io::Result<UhidPen> {
        let path = std::ffi::CString::new(UHID_PATH).unwrap();
        let raw = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let descriptor = descriptor();
        let mut create = Vec::with_capacity(4 + 128 + 64 + 64 + 2 + 2 + 16 + descriptor.len());
        create.extend_from_slice(&UHID_CREATE2.to_le_bytes());
        let padded = |text: &str, n: usize| { let mut b = text.as_bytes().to_vec(); b.resize(n, 0); b };
        create.extend_from_slice(&padded(DEVICE_NAME, 128));
        create.extend_from_slice(&padded("plus-one", 64));
        create.extend_from_slice(&padded("", 64));
        create.extend_from_slice(&(descriptor.len() as u16).to_le_bytes());
        create.extend_from_slice(&BUS_VIRTUAL.to_le_bytes());
        for value in [VENDOR, PRODUCT, 1, 0] {
            create.extend_from_slice(&value.to_le_bytes());
        }
        create.extend_from_slice(&descriptor);
        write_all(&fd, &create)?;
        let fd = Arc::new(Mutex::new(Some(fd)));
        let closing = Arc::new(AtomicBool::new(false));
        // The kernel queues start/open notifications: drained, so the queue never fills up.
        let (drained, stop) = (fd.clone(), closing.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 4380];
            while !stop.load(Ordering::Relaxed) {
                let raw = match drained.lock().unwrap().as_ref() { Some(fd) => fd.as_raw_fd(), None => break };
                let mut poll = libc::pollfd { fd: raw, events: libc::POLLIN, revents: 0 };
                if unsafe { libc::poll(&mut poll, 1, 500) } > 0 {
                    unsafe { libc::read(raw, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                }
            }
            drained.lock().unwrap().take();  // closed here
        });
        log!("pen: virtual HID tablet created (pressure and tilt)");
        Ok(UhidPen { fd, closing })
    }
}

fn write_all(fd: &OwnedFd, data: &[u8]) -> std::io::Result<()> {
    let n = unsafe { libc::write(fd.as_raw_fd(), data.as_ptr() as *const libc::c_void, data.len()) };
    if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

impl Pen for UhidPen {
    fn report(&mut self, data: &[u8]) {
        if is_report(data) {
            let fd = self.fd.lock().unwrap();
            if let (Some(fd), false) = (fd.as_ref(), self.closing.load(Ordering::Relaxed)) {
                let mut event = Vec::with_capacity(6 + data.len());
                event.extend_from_slice(&UHID_INPUT2.to_le_bytes());
                event.extend_from_slice(&(data.len() as u16).to_le_bytes());
                event.extend_from_slice(data);
                let _ = write_all(fd, &event);
            }
        }
    }

    fn close(&mut self) {
        if self.closing.swap(true, Ordering::Relaxed) {
            return;
        }
        if let Some(fd) = self.fd.lock().unwrap().as_ref() {
            let _ = write_all(fd, &UHID_DESTROY.to_le_bytes());
        }
    }
}

/// Fallback: hover moves the pointer, tip = left button, barrel = right.
struct PointerPen {
    injector: Arc<dyn Injector>,
    size: (f64, f64),
    buttons: u8,
}

impl Pen for PointerPen {
    fn report(&mut self, data: &[u8]) {
        if !is_report(data) {
            return;
        }
        let bits = data[1];
        let (x, y) = (u16::from_le_bytes([data[2], data[3]]) as f64, u16::from_le_bytes([data[4], data[5]]) as f64);
        if bits & IN_RANGE != 0 {
            self.injector.pointer_move(x * self.size.0 / AXIS_MAX, y * self.size.1 / AXIS_MAX);
        }
        for (mask, button) in [(TIP, BTN_LEFT), (BARREL, BTN_RIGHT)] {
            let pressed = bits & mask != 0 && bits & IN_RANGE != 0;
            if pressed != (self.buttons & mask != 0) {
                self.injector.pointer_button(button, pressed);
                self.buttons ^= mask;
            }
        }
    }

    fn close(&mut self) {
        for (mask, button) in [(TIP, BTN_LEFT), (BARREL, BTN_RIGHT)] {
            if self.buttons & mask != 0 {
                self.injector.pointer_button(button, false);
            }
        }
        self.buttons = 0;
    }
}

/// Tells Mutter to map our tablet onto the monitor with this EDID.
fn map_to_monitor(edid: &(String, String, String)) -> bool {
    let Some(source) = gio::SettingsSchemaSource::default() else { return false };
    if source.lookup(TABLET_SCHEMA, true).is_none() {
        log!("pen: no tablet settings schema, cannot map the pen to the monitor");
        return false;
    }
    let path = format!("/org/gnome/desktop/peripherals/tablets/{VENDOR:04x}:{PRODUCT:04x}/");
    let settings = gio::Settings::with_path(TABLET_SCHEMA, &path);
    let output = [edid.0.as_str(), edid.1.as_str(), edid.2.as_str()];
    let ok = settings.set_strv("output", output).is_ok() && settings.set_boolean("keep-aspect", false).is_ok();
    gio::Settings::sync();
    log!("pen: mapped to monitor {edid:?}");
    ok
}

/// The S Pen for the tablet's virtual monitor: a pen tablet (pressure,
/// tilt) mapped onto it, else the pointer through the monitor's session. A
/// pen tablet is the whole desktop's until mapped: not mapped, it would
/// point and click on the PC's own screens, so then the pointer.
pub fn for_monitor(injector: Arc<dyn Injector>, size: (f64, f64)) -> Box<dyn Pen> {
    if uhid_available() {
        let mapped = virtual_monitor_edid().is_some_and(|edid| map_to_monitor(&edid));
        if mapped {
            if let Some(pen) = helper_pen() {
                return pen;
            }
            if uhid_writable() {
                match UhidPen::open() {
                    Ok(pen) => return Box::new(pen),
                    Err(e) => log!("pen: /dev/uhid: {e}"),
                }
            }
        } else {
            log!("pen: the tablet's monitor not known to map it onto: the pointer instead");
        }
    }
    log!("pen: no access to /dev/uhid, the pen drives the pointer (no pressure)");
    Box::new(PointerPen { injector, size, buttons: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_is_pen_py_s() {
        let d = descriptor();
        assert_eq!(d.len(), S_PEN.len() / 2);
        assert_eq!(&d[..4], &[0x05, 0x0d, 0x09, 0x02]);  // Digitizer page, Pen
        assert_eq!(*d.last().unwrap(), 0xc0);
    }
}
