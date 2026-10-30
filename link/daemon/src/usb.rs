//! The tablet by cable: Android Open Accessory (as Android Auto by cable),
//! through usbfs (no libusb). A tablet plugged in is asked to become an
//! accessory of Plus One's (three control requests); it comes back as
//! Google's accessory device (18d1:2d00, or 2d01 with adb), whose two bulk
//! endpoints are a byte stream: bridged to a socket, the session runs on it
//! as on TCP. The app's side: an Activity Android starts for the accessory
//! (UsbActivity.kt), which hands it to the link's core.
//!
//! Only a tablet asked for: one whose USB serial was reached this way
//! before (remembered), or any Android device when an app asks (the "usb"
//! request: the user's "use the cable"); each plug asked once.

use plusone_link::log;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const SYSFS: &str = "/sys/bus/usb/devices";
const GOOGLE: &str = "18d1";
const ACCESSORY: [&str; 2] = ["2d00", "2d01"];  // accessory; accessory and adb
const SAMSUNG: &str = "04e8";
/// Interface classes no tablet is made of alone: audio (01), HID (03),
/// printer (07), mass storage (08), hub (09), video (0e).
const NOT_A_TABLET: [&str; 6] = ["01", "03", "07", "08", "09", "0e"];
const GET_PROTOCOL: u8 = 51;
const SEND_STRING: u8 = 52;
const START: u8 = 53;
/// What the app's accessory filter matches (res/xml/accessory_filter.xml).
const STRINGS: [&str; 6] = ["Plus One", "Plus One", "The tablet as the PC's second screen", "1",
                            "https://github.com/GabrieleSantangelo/PlusOne", ""];
/// A transfer each way: four of the accessory's own (f_accessory's
/// BULK_BUFFER_SIZE, 16 KiB), which it takes one after the other, so a
/// quarter of the calls for the same bytes (a call costs about as much as
/// a 16 KiB transfer).
const CHUNK: usize = 64 << 10;
/// Of the speed its USB link was made at (sysfs's speed), what a cable is
/// taken to carry each way until what it drains is measured (the session's
/// prior for it, Stream::prior): measured (2026-10-27) at USB 2.0's high
/// speed, 480 Mbit/s, 17.5 MB/s one way and 18.7 each way at once, 0.29
/// of it (the accessory reads 16 KiB at a time); a fifth, 12 MB/s (96
/// Mbit/s), at high speed; at USB 3's 5 Gbit/s 125 MB/s, not measured: its
/// drain then says (a cable has no congestion, only the tablet's reading).
const AOA_SHARE: f64 = 0.2;
/// The app's READY waited for with no timeout (usbfs's 0): nothing wakes
/// meanwhile, however long no app answers; the cable pulled ends the read
/// (usbfs kills the transfer: an error).
const READY_WAIT: u32 = 0;
/// An accessory that could not be opened (its interface held by another,
/// its endpoints not there yet), its session not made, or its thread not
/// made (Watcher::unspawned): tried again
/// after this, twice as long after each failure in a row, up to AGAIN_MOST
/// (it was tried each second, and each failure logged: 68 lines of
/// "Device or resource busy" in a minute).
const AGAIN: Duration = Duration::from_secs(1);
const AGAIN_MOST: Duration = Duration::from_secs(16);
const USBDEVFS_CONTROL: libc::c_ulong = 0xC018_5500;
const USBDEVFS_BULK: libc::c_ulong = 0xC018_5502;
const USBDEVFS_CLAIMINTERFACE: libc::c_ulong = 0x8004_550F;

#[repr(C)]
struct Control {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    timeout: u32,
    data: *mut libc::c_void,
}

#[repr(C)]
struct Bulk {
    endpoint: u32,
    length: u32,
    timeout: u32,
    data: *mut libc::c_void,
}

/// A USB device, as sysfs tells it.
#[derive(Clone, Debug)]
pub struct Device {
    pub path: PathBuf,
    pub vendor: String,
    pub product: String,
    pub serial: String,
    bus: u32,
    number: u32,
    /// The speed its USB link was made at (Mbit/s: 480 at USB 2's high
    /// speed, 5000 and more at USB 3's), if sysfs says.
    pub speed: Option<f64>,
}

impl Device {
    fn node(&self) -> String {
        format!("/dev/bus/usb/{:03}/{:03}", self.bus, self.number)
    }

    /// What a cable at its speed is taken to carry each way until what it
    /// drains is measured (bytes a second): AOA_SHARE of it; None if its
    /// speed is not known, or not one a link is made at (the session's own
    /// prior for a cable then).
    pub fn prior(&self) -> Option<u64> {
        self.speed.filter(|speed| speed.is_finite() && *speed > 0.0).map(|speed| (speed * 1e6 / 8.0 * AOA_SHARE) as u64)
            .filter(|prior| *prior > 0)
    }

    pub fn is_accessory(&self) -> bool {
        self.vendor == GOOGLE && ACCESSORY.contains(&self.product.as_str())
    }

    /// An Android device, maybe: Samsung's, or one with an adb or MTP/PTP
    /// interface. A tablet charging only may show neither adb nor MTP, and
    /// switches all the same (the accessory's requests work whatever its
    /// USB mode); but not a device whose every interface is of a kind no
    /// tablet is made of alone (a Samsung disk or hub, a keyboard, a
    /// webcam, a printer).
    pub fn is_android(&self) -> bool {
        if self.is_accessory() {
            return false;
        }
        let classes: Vec<(String, String)> = interfaces(&self.path).iter()
            .map(|i| (read(&i.join("bInterfaceClass")), read(&i.join("bInterfaceSubClass")))).collect();
        if !classes.is_empty() && classes.iter().all(|(class, _)| NOT_A_TABLET.contains(&class.as_str())) {
            return false;
        }
        self.vendor == SAMSUNG || classes.iter().any(|(class, subclass)| class == "06" || (class == "ff" && subclass == "42"))
    }

    fn open(&self) -> io::Result<OwnedFd> {
        let path = std::ffi::CString::new(self.node()).unwrap();
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).map(|s| s.trim().to_owned()).unwrap_or_default()
}

fn interfaces(device: &Path) -> Vec<PathBuf> {
    let name = device.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
    fs::read_dir(device).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(&format!("{name}:"))))
        .collect()
}

/// The USB devices plugged in now (not the hubs').
pub fn devices() -> Vec<Device> {
    let mut found = Vec::new();
    for entry in fs::read_dir(SYSFS).into_iter().flatten().flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.contains(':') || name.starts_with("usb") {
            continue;
        }
        let (Ok(bus), Ok(number)) = (read(&path.join("busnum")).parse(), read(&path.join("devnum")).parse()) else { continue };
        found.push(Device { vendor: read(&path.join("idVendor")), product: read(&path.join("idProduct")),
                            serial: read(&path.join("serial")), bus, number, speed: read(&path.join("speed")).parse().ok(),
                            path });
    }
    found
}

fn control(fd: &OwnedFd, request_type: u8, request: u8, index: u16, data: &mut [u8]) -> io::Result<usize> {
    let mut transfer = Control { request_type, request, value: 0, index, length: data.len() as u16, timeout: 1000,
                                 data: data.as_mut_ptr() as *mut libc::c_void };
    let done = unsafe { libc::ioctl(fd.as_raw_fd(), USBDEVFS_CONTROL, &mut transfer) };
    if done < 0 { Err(io::Error::last_os_error()) } else { Ok(done as usize) }
}

/// device asked to become Plus One's accessory: it leaves the bus, and
/// comes back as one (devices() then finds it).
pub fn switch(device: &Device) -> io::Result<()> {
    let fd = device.open()?;
    let mut version = [0u8; 2];
    control(&fd, 0xC0, GET_PROTOCOL, 0, &mut version)?;
    if u16::from_le_bytes(version) == 0 {
        return Err(io::Error::other("no accessory mode"));
    }
    for (index, text) in STRINGS.iter().enumerate() {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        control(&fd, 0x40, SEND_STRING, index as u16, &mut bytes)?;
    }
    control(&fd, 0x40, START, 0, &mut [])?;
    Ok(())
}

/// The accessory's device, claimed, while its bridge's threads run: let
/// go of (closed, its interface with it) as the last of them ends, and
/// then said (Released).
struct Claimed {
    fd: OwnedFd,
    _said: mpsc::Sender<()>,
}

/// Said once the accessory is let go of: its bridge's threads both over
/// (the session's stream on it ended, or the cable pulled).
pub struct Released(mpsc::Receiver<()>);

impl Released {
    /// One that says so as the last clone of claimed goes: (it, the
    /// receiver's side).
    fn new() -> (mpsc::Sender<()>, Released) {
        let (said, released) = mpsc::channel();
        (said, Released(released))
    }

    /// Until the accessory is let go of.
    pub fn wait(self) {
        let _ = self.0.recv();
    }
}

/// The accessory's byte stream, bridged to a socket for a session, and
/// when its device is let go of: not before the bridge's threads end,
/// whatever became of the session meanwhile (the next attempt claims it
/// again only then).
pub fn open_accessory(device: &Device) -> io::Result<(UnixStream, Released)> {
    let interface = interfaces(&device.path).into_iter().find(|i| i.to_string_lossy().ends_with(":1.0"))
        .ok_or_else(|| io::Error::other("no interface"))?;
    let (mut inward, mut outward) = (None, None);
    for endpoint in fs::read_dir(&interface)?.flatten().map(|e| e.path()) {
        if !endpoint.file_name().is_some_and(|n| n.to_string_lossy().starts_with("ep_")) || read(&endpoint.join("type")) != "Bulk" {
            continue;
        }
        let address = u32::from_str_radix(&read(&endpoint.join("bEndpointAddress")), 16).unwrap_or(0);
        if read(&endpoint.join("direction")) == "in" { inward = Some(address) } else { outward = Some(address) }
    }
    let (Some(inward), Some(outward)) = (inward, outward) else { return Err(io::Error::other("no bulk endpoints")) };
    let (said, released) = Released::new();
    let fd = Arc::new(Claimed { fd: device.open()?, _said: said });
    let mut number: libc::c_uint = 0;
    if unsafe { libc::ioctl(fd.fd.as_raw_fd(), USBDEVFS_CLAIMINTERFACE, &mut number) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let bulk = |fd: &Claimed, endpoint: u32, data: *mut u8, length: usize, timeout: u32| -> io::Result<usize> {
        let mut transfer = Bulk { endpoint, length: length as u32, timeout, data: data as *mut libc::c_void };
        let done = unsafe { libc::ioctl(fd.fd.as_raw_fd(), USBDEVFS_BULK, &mut transfer) };
        if done < 0 { Err(io::Error::last_os_error()) } else { Ok(done as usize) }
    };
    let number = ready(|buf, timeout| bulk(&fd, inward, buf.as_mut_ptr(), buf.len(), timeout))?;
    let mut go = plusone_link::stream::USB_GO.to_vec();
    go.extend_from_slice(&number);
    bulk(&fd, outward, go.as_mut_ptr(), go.len(), 2000)?;
    let reader = fd.clone();
    // Its READYs sent before it saw the GO: dropped, until the session's first bytes.
    let mut started = false;
    let stream = plusone_link::stream::bridge(
        // A second without anything: TimedOut, the bridge only looks whether the session is still there.
        move |buf| loop {
            let n = bulk(&reader, inward, buf.as_mut_ptr(), buf.len(), 1000)?;
            if !started && buf[..n].starts_with(plusone_link::stream::USB_READY) {
                continue;
            }
            started = true;
            return Ok(n);
        },
        move |data| {
            for piece in plusone_link::stream::usb_pieces(data) {
                let mut sent = 0;
                while sent < piece.len() {
                    sent += bulk(&fd, outward, piece[sent..].as_ptr() as *mut u8, piece.len() - sent, 2000)?;
                }
            }
            Ok(())
        },
        CHUNK)?;
    Ok((stream, released))
}

/// The app's READY on the accessory, read(buf, timeout in ms) reading it:
/// its number. The app first: it says so once it has the accessory open
/// (the user may still be answering Android's question), and again until
/// answered. Until then, or the cable gone, nothing is sent.
fn ready(mut read: impl FnMut(&mut [u8], u32) -> io::Result<usize>) -> io::Result<[u8; 8]> {
    let mut buf = vec![0u8; CHUNK];
    loop {
        match read(&mut buf, READY_WAIT) {
            Ok(n) => {
                if let Some(number) = plusone_link::stream::usb_number(plusone_link::stream::USB_READY, &buf[..n]) {
                    return Ok(number);
                }
                // An attempt's leftovers from before: dropped.
            }
            Err(e) if e.kind() == io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(e),
        }
    }
}

/// The cable, watched: accessories bridged to sessions (connect: the
/// session made on the stream; its device id or why not), Android devices
/// switched when wanted (remembered serials; any while asked to).
pub struct Watcher {
    bridged: Mutex<HashSet<(u32, u32)>>,
    /// Plugs asked to switch already: one still there did not (refused,
    /// no accessory mode), and asking again each EVERY changes nothing.
    /// Forgotten as it leaves the bus (Android comes back with another
    /// number at each change of its USB mode), or as an app asks.
    tried: Mutex<HashSet<(u32, u32)>>,
    /// Accessories whose thread could not be made (the system out of
    /// them): when each is tried again, and how long after the next
    /// failure (AGAIN, twice as long each time in a row, AGAIN_MOST); said
    /// once. (Never tried again while it stayed plugged in: no session by
    /// cable.)
    unspawned: Mutex<HashMap<(u32, u32), (Instant, Duration)>>,
    pub asked: std::sync::atomic::AtomicBool,
}

/// What makes a thread for an accessory (std's, named; a test's own).
type Spawn<'a> = &'a dyn Fn(Box<dyn FnOnce() + Send>) -> io::Result<()>;

impl Watcher {
    pub fn new() -> Watcher {
        Watcher { bridged: Mutex::default(), tried: Mutex::default(), unspawned: Mutex::default(),
                  asked: std::sync::atomic::AtomicBool::new(false) }
    }

    /// The user's "use the cable" (or no longer): each plug asked again.
    pub fn ask(&self, on: bool) {
        self.asked.store(on, std::sync::atomic::Ordering::Relaxed);
        self.tried.lock().unwrap().clear();
    }

    /// Once: what is plugged in now, done; an accessory on a thread of its
    /// own (it waits for the app). remembered: serials reached by cable before.
    pub fn look(&self, remembered: &dyn Fn(&str) -> bool, switched: &dyn Fn(&Device),
                connect: std::sync::Arc<dyn Fn(std::os::unix::net::UnixStream, &Device) -> Result<String, String> + Send + Sync>) {
        self.look_at(devices(), &switch, remembered, switched, connect,
                     &|run| std::thread::Builder::new().name("link-usb-accessory".into()).spawn(run).map(|_| ()));
    }

    /// As look(), plugged in: those, each asked to switch by switch, each
    /// accessory's thread made by spawn.
    fn look_at(&self, plugged: Vec<Device>, switch: &dyn Fn(&Device) -> io::Result<()>, remembered: &dyn Fn(&str) -> bool,
               switched: &dyn Fn(&Device),
               connect: std::sync::Arc<dyn Fn(std::os::unix::net::UnixStream, &Device) -> Result<String, String> + Send + Sync>,
               spawn: Spawn) {
        let asked = self.asked.load(std::sync::atomic::Ordering::Relaxed);
        for device in plugged.iter().cloned() {
            if device.is_accessory() {
                let key = (device.bus, device.number);
                if self.unspawned.lock().unwrap().get(&key).is_some_and(|(next, _)| Instant::now() < *next)
                    || !self.bridged.lock().unwrap().insert(key) {
                    continue;
                }
                let (connect, node) = (connect.clone(), device.node());
                let made = spawn(Box::new(move || {
                    sessions_on(&device, &|| devices().iter().any(|d| (d.bus, d.number) == (device.bus, device.number)),
                                &|| open_accessory(&device), &*connect, &std::thread::sleep, &|said| log!("{said}"));
                }));
                let mut unspawned = self.unspawned.lock().unwrap();
                match made {
                    Ok(()) => {
                        unspawned.remove(&key);
                    }
                    Err(e) => {
                        self.bridged.lock().unwrap().remove(&key);
                        let again = match unspawned.get(&key) {
                            Some((_, again)) => *again,
                            None => {
                                log!("usb: the accessory at {node}: no thread for it ({e}), tried again");
                                AGAIN
                            }
                        };
                        unspawned.insert(key, (Instant::now() + again, (again * 2).min(AGAIN_MOST)));
                    }
                }
            } else if device.is_android() && (asked || (!device.serial.is_empty() && remembered(&device.serial)))
                && self.tried.lock().unwrap().insert((device.bus, device.number)) {
                match switch(&device) {
                    Ok(()) => {
                        log!("usb: {} {}:{} asked to be Plus One's accessory", device.serial, device.vendor, device.product);
                        switched(&device);
                    }
                    Err(e) => log!("usb: {}: {e}", device.node()),
                }
            }
        }
        // Gone from the bus: forgotten (the same numbers can come again).
        let present: HashSet<(u32, u32)> = plugged.iter().map(|d| (d.bus, d.number)).collect();
        self.bridged.lock().unwrap().retain(|key| present.contains(key));
        self.tried.lock().unwrap().retain(|key| present.contains(key));
        self.unspawned.lock().unwrap().retain(|key, _| present.contains(key));
    }
}

/// Sessions on an accessory, one after the other, as long as it is plugged
/// in (present): each on what open gives (its stream, and when its device
/// is let go of), connect making the session (its device id, or why not).
/// The next attempt only once the device is let go of: the session's
/// stream on it ended (the session over, or the path let go of), not as
/// soon as connect returns (the session goes on over the cable, a reserve
/// or the carrier, long after: claimed again meanwhile, the accessory was
/// "Device or resource busy" each second until the cable was pulled). A
/// failure (open's, connect's) tried again after AGAIN, twice as long each
/// time in a row up to AGAIN_MOST (pause), and said once until it changes
/// (say).
fn sessions_on(device: &Device, present: &dyn Fn() -> bool, open: &dyn Fn() -> io::Result<(UnixStream, Released)>,
               connect: &(dyn Fn(UnixStream, &Device) -> Result<String, String> + Send + Sync),
               pause: &dyn Fn(Duration), say: &dyn Fn(String)) {
    let mut again = AGAIN;
    let mut said: Option<String> = None;
    while present() {
        let failed = match open() {
            Ok((stream, released)) => {
                let made = connect(stream, device);
                released.wait();
                match made {
                    Ok(id) => {
                        say(format!("usb: a session with {} by cable, over", &id[..8.min(id.len())]));
                        (again, said) = (AGAIN, None);
                        continue;
                    }
                    Err(e) => e,
                }
            }
            Err(e) => e.to_string(),
        };
        if said.as_deref() != Some(failed.as_str()) {
            say(format!("usb: the accessory at {}: {failed}", device.node()));
            said = Some(failed);
        }
        pause(again);
        again = (again * 2).min(AGAIN_MOST);
    }
}

/// How often the bus is looked at.
pub const EVERY: Duration = Duration::from_secs(2);

/// An accessory at speed (Mbit/s), for a test of plus-one-link's.
#[cfg(test)]
pub fn accessory_at(speed: Option<f64>) -> Device {
    Device { path: PathBuf::from("/nonexistent"), vendor: GOOGLE.into(), product: ACCESSORY[0].into(), serial: String::new(),
             bus: 3, number: 28, speed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// A device in a sysfs of the test's own: its interfaces (class,
    /// subclass, protocol, name).
    fn device(name: &str, vendor: &str, number: u32, interfaces: &[(&str, &str, &str, &str)]) -> Device {
        let path = std::env::temp_dir().join(format!("plus-one-usb-{}", std::process::id())).join(name);
        let _ = fs::remove_dir_all(&path);
        for (i, (class, subclass, protocol, named)) in interfaces.iter().enumerate() {
            let interface = path.join(format!("{name}:1.{i}"));
            fs::create_dir_all(&interface).unwrap();
            for (file, value) in [("bInterfaceClass", class), ("bInterfaceSubClass", subclass), ("bInterfaceProtocol", protocol)] {
                fs::write(interface.join(file), format!("{value}\n")).unwrap();
            }
            if !named.is_empty() {
                fs::write(interface.join("interface"), format!("{named}\n")).unwrap();
            }
        }
        Device { path, vendor: vendor.into(), product: "0001".into(), serial: format!("S{number}"), bus: 2, number,
                 speed: None }
    }

    #[test]
    fn a_cable_is_taken_to_carry_a_fifth_of_its_speed() {
        // USB 2's high speed: 12 MB/s (96 Mbit/s; it carries 17.5 one way).
        let mut tablet = device("2-12", GOOGLE, 12, &[]);
        // What sysfs says ("1.5", "12", "480", "5000", "10000", "20000";
        // "unknown": None), and what it never says.
        for (speed, prior) in [(Some(480.0), Some(12_000_000)), (Some(5000.0), Some(125_000_000)), (Some(12.0), Some(300_000)),
                               (Some(1.5), Some(37_500)), (Some(10000.0), Some(250_000_000)), (Some(20000.0), Some(500_000_000)),
                               (None, None), (Some(0.0), None), (Some(-480.0), None), (Some(1e-9), None),
                               (Some(f64::INFINITY), None), (Some(f64::NAN), None)] {
            tablet.speed = speed;
            assert_eq!(tablet.prior(), prior, "{speed:?}");
        }
    }

    #[test]
    fn a_device_no_tablet_is_made_of_is_no_android() {
        // Samsung's vendor id alone made a device Android: a Samsung disk
        // or hub was asked to switch, and failed, every EVERY while "use
        // the cable" was on. A tablet with no adb nor MTP (charging only)
        // is still one to ask.
        assert!(!device("2-1", "04e8", 1, &[("08", "06", "50", "")]).is_android(), "a Samsung disk");
        assert!(!device("2-2", "04e8", 2, &[("09", "00", "00", "")]).is_android(), "a Samsung hub");
        assert!(!device("2-3", "046d", 3, &[("0e", "01", "00", ""), ("01", "01", "00", "")]).is_android(), "a webcam");
        assert!(device("2-4", "04e8", 4, &[("ff", "ff", "00", "")]).is_android(), "a Samsung tablet charging only");
        assert!(device("2-5", "04e8", 5, &[]).is_android(), "a Samsung tablet showing no interface");
        assert!(device("2-6", "04e8", 6, &[("ff", "ff", "00", "MTP"), ("ff", "42", "01", "ADB Interface")]).is_android(),
                "a Samsung tablet, MTP and adb");
        assert!(device("2-7", "18d1", 7, &[("ff", "42", "01", "")]).is_android(), "another's tablet, adb");
        assert!(!device("2-8", "18d1", 8, &[("ff", "ff", "00", "")]).is_android(), "another's, no adb nor PTP");
    }

    #[test]
    fn a_device_that_does_not_switch_is_asked_once_each_plug() {
        // Asked again every EVERY for as long as it stayed plugged in, each
        // failure logged.
        let watcher = Watcher::new();
        watcher.ask(true);
        let tablet = device("2-9", "04e8", 9, &[("ff", "ff", "00", "")]);
        let asked = std::cell::Cell::new(0);
        let switch = |_: &Device| -> io::Result<()> {
            asked.set(asked.get() + 1);
            Err(io::Error::other("no accessory mode"))
        };
        let connect: std::sync::Arc<dyn Fn(std::os::unix::net::UnixStream, &Device) -> Result<String, String> + Send + Sync> =
            std::sync::Arc::new(|_, _| Err("no".into()));
        let look = |plugged: Vec<Device>| watcher.look_at(plugged, &switch, &|_| false, &|_| {}, connect.clone(), &|_| Ok(()));
        for _ in 0..3 {
            look(vec![tablet.clone()]);
        }
        assert_eq!(asked.get(), 1, "asks while it stayed plugged in");
        look(Vec::new());
        look(vec![tablet.clone()]);
        assert_eq!(asked.get(), 2, "plugged in again, asked again");
        watcher.ask(true);
        look(vec![tablet.clone()]);
        assert_eq!(asked.get(), 3, "an app asking, asked again");
    }

    #[test]
    fn waiting_for_the_app_wakes_for_nothing() {
        // An accessory with no app answering: its READY waited for in reads
        // of a second each, for as long as it stayed plugged in (a wakeup
        // each second). Read as usbfs reads: one with a timeout ends with
        // it, one with none only with something read or the cable pulled
        // (here after 2 s).
        let started = Instant::now();
        let pulled = Duration::from_secs(2);
        let mut reads = 0;
        let result = ready(|_, timeout| {
            reads += 1;
            let left = pulled.saturating_sub(started.elapsed());
            if timeout != 0 && Duration::from_millis(timeout.into()) < left {
                std::thread::sleep(Duration::from_millis(timeout.into()));
                return Err(io::ErrorKind::TimedOut.into());
            }
            std::thread::sleep(left);
            Err(io::Error::from_raw_os_error(libc::ENODEV))
        });
        assert_eq!(result.err().and_then(|e| e.raw_os_error()), Some(libc::ENODEV));
        assert_eq!(reads, 1, "reads while no app answered, for 2 s");
    }

    /// An accessory as open_accessory() gives it, faked: claimed (a second
    /// claim busy) until both threads of its bridge are over, the app's end
    /// kept in apps (silent; its end would be read as zero-length packets).
    fn fake_accessory(claimed: &Arc<std::sync::atomic::AtomicBool>, apps: &std::cell::RefCell<Vec<UnixStream>>)
                      -> io::Result<(UnixStream, Released)> {
        use std::io::{Read, Write};
        use std::sync::atomic::Ordering;
        struct Claim {
            claimed: Arc<std::sync::atomic::AtomicBool>,
            _said: mpsc::Sender<()>,
        }
        impl Drop for Claim {
            fn drop(&mut self) {
                self.claimed.store(false, Ordering::SeqCst);
            }
        }
        if claimed.swap(true, Ordering::SeqCst) {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }
        let (said, released) = Released::new();
        let claim = Arc::new(Claim { claimed: claimed.clone(), _said: said });
        let (device, app) = UnixStream::pair()?;
        apps.borrow_mut().push(app);
        device.set_read_timeout(Some(Duration::from_millis(100)))?;
        let (mut reading, mut writing) = (device.try_clone()?, device);
        let (for_reading, for_writing) = (claim.clone(), claim);
        let stream = plusone_link::stream::bridge(
            move |buf| {
                let _claim = &for_reading;
                reading.read(buf).map_err(|e| if e.kind() == io::ErrorKind::WouldBlock { io::ErrorKind::TimedOut.into() } else { e })
            },
            move |data| {
                let _claim = &for_writing;
                writing.write_all(data)
            },
            4096)?;
        Ok((stream, released))
    }

    #[test]
    fn an_accessory_is_claimed_again_only_once_its_session_let_it_go() {
        // The session made on the cable goes on (a reserve, the carrier)
        // long after connect returns, its stream holding the accessory:
        // the next attempt waits for it to be let go of. (It came at once,
        // connect given up 5 s on: "Device or resource busy" each second
        // until the cable was pulled.)
        let tablet = device("2-10", GOOGLE, 10, &[]);
        let claimed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let apps = std::cell::RefCell::new(Vec::new());
        let opened = std::cell::RefCell::new(Vec::new());
        let made = std::sync::atomic::AtomicUsize::new(0);
        let said = std::cell::RefCell::new(Vec::new());
        let started = Instant::now();
        let held = Duration::from_millis(1500);
        sessions_on(&tablet, &|| made.load(std::sync::atomic::Ordering::SeqCst) < 2,
                    &|| {
                        let accessory = fake_accessory(&claimed, &apps);
                        opened.borrow_mut().push((started.elapsed(), accessory.is_ok()));
                        accessory
                    },
                    &|stream, _| {
                        made.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        // The session on it until held is over; made at once.
                        std::thread::spawn(move || {
                            std::thread::sleep(held);
                            drop(stream);
                        });
                        Ok("6a2665bcebe1984c".repeat(4))
                    },
                    &std::thread::sleep, &|line| said.borrow_mut().push(line));
        let opened = opened.into_inner();
        assert!(opened.iter().all(|(_, ok)| *ok), "claimed while held: {opened:?}, said {:?}", said.borrow());
        assert_eq!(opened.len(), 2, "{opened:?}");
        assert!(opened[1].0 >= held, "claimed again {:?} in, the first held {held:?}", opened[1].0);
        assert_eq!(said.into_inner(), ["usb: a session with 6a2665bc by cable, over"; 2]);
        assert!(!claimed.load(std::sync::atomic::Ordering::SeqCst), "the accessory still claimed at the end");
    }

    #[test]
    fn an_accessory_that_fails_is_tried_less_and_less_and_said_once() {
        // Busy for a minute (another holding it): tried after 1 s, then 2,
        // 4, 8, 16, 16... and said once. (Tried and said each second: 68
        // lines in a minute.)
        let tablet = device("2-11", GOOGLE, 11, &[]);
        let clock = std::cell::Cell::new(Duration::ZERO);
        let tries = std::cell::Cell::new(0);
        let said = std::cell::RefCell::new(Vec::new());
        sessions_on(&tablet, &|| clock.get() < Duration::from_secs(60),
                    &|| {
                        tries.set(tries.get() + 1);
                        Err(io::Error::from_raw_os_error(libc::EBUSY))
                    },
                    &|_, _| unreachable!("no stream, no session"),
                    &|pause| clock.set(clock.get() + pause), &|line| said.borrow_mut().push(line));
        assert_eq!(tries.get(), 7, "tries in a minute (at 0, 1, 3, 7, 15, 31, 47 s)");
        let said = said.into_inner();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].starts_with("usb: the accessory at /dev/bus/usb/002/011: ") && said[0].contains("busy"), "{said:?}");
    }

    /// An accessory as open_accessory() gives it, faked, on a cable pulled
    /// once pulled is: claimed until both threads of its bridge are over
    /// (each holding a clone of alive meanwhile); silent, its reads only
    /// time out, until pulled (ENODEV, as usbfs says it).
    fn pullable_accessory(claimed: &Arc<std::sync::atomic::AtomicBool>, pulled: &Arc<std::sync::atomic::AtomicBool>,
                          alive: &Arc<()>) -> io::Result<(UnixStream, Released)> {
        use std::sync::atomic::Ordering;
        struct Claim {
            claimed: Arc<std::sync::atomic::AtomicBool>,
            _said: mpsc::Sender<()>,
        }
        impl Drop for Claim {
            fn drop(&mut self) {
                self.claimed.store(false, Ordering::SeqCst);
            }
        }
        let gone = || io::Error::from_raw_os_error(libc::ENODEV);
        if pulled.load(Ordering::SeqCst) {
            return Err(gone());
        }
        if claimed.swap(true, Ordering::SeqCst) {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }
        let (said, released) = Released::new();
        let claim = Arc::new(Claim { claimed: claimed.clone(), _said: said });
        let reading = (claim.clone(), pulled.clone(), alive.clone());
        let writing = (claim, pulled.clone(), alive.clone());
        let stream = plusone_link::stream::bridge(
            move |_| {
                let (_claim, pulled, _alive) = &reading;
                std::thread::sleep(Duration::from_millis(10));
                Err(if pulled.load(Ordering::SeqCst) { gone() } else { io::ErrorKind::TimedOut.into() })
            },
            move |_| {
                let (_claim, pulled, _alive) = &writing;
                if pulled.load(Ordering::SeqCst) { Err(gone()) } else { Ok(()) }
            },
            4096)?;
        Ok((stream, released))
    }

    #[test]
    fn twenty_plugs_leave_no_thread_and_no_claim() {
        // Twenty plugs, each on a thread of its own as look() makes them,
        // and pulled: in a session, after one ended (waiting for the app's
        // next READY), or after one not made (waiting to try again). Each
        // plug's thread over, its accessory let go of, no bridge's thread
        // left, nor a session's stream on it.
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let alive = Arc::new(());
        let sessions = Arc::new(Mutex::new(Vec::new()));
        for plug in 0..20usize {
            let (claimed, pulled, connected) =
                (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
            let (done, over) = mpsc::channel();
            {
                let (claimed, pulled, connected, alive, sessions) =
                    (claimed.clone(), pulled.clone(), connected.clone(), alive.clone(), sessions.clone());
                std::thread::spawn(move || {
                    let opened = AtomicUsize::new(0);
                    sessions_on(&accessory_at(None), &|| !pulled.load(Ordering::SeqCst),
                                &|| {
                                    if opened.fetch_add(1, Ordering::SeqCst) > 0 && plug % 3 == 1 {
                                        // Waiting for the app's READY, the cable pulled.
                                        pulled.store(true, Ordering::SeqCst);
                                    }
                                    pullable_accessory(&claimed, &pulled, &alive)
                                },
                                &|stream, _| {
                                    connected.store(true, Ordering::SeqCst);
                                    match plug % 3 {
                                        // The session on it until its stream ends (the cable pulled).
                                        0 => sessions.lock().unwrap().push(std::thread::spawn(move || {
                                            let mut stream = stream;
                                            let _ = std::io::Read::read(&mut stream, &mut [0u8; 64]);
                                        })),
                                        // The session over a moment on, the cable still in.
                                        1 => sessions.lock().unwrap().push(std::thread::spawn(move || {
                                            std::thread::sleep(Duration::from_millis(30));
                                            drop(stream);
                                        })),
                                        // Not made: its stream dropped at once.
                                        _ => return Err("the handshake failed".into()),
                                    }
                                    Ok("6a2665bcebe1984c".repeat(4))
                                },
                                // Waiting to try again, the cable pulled.
                                &|_| pulled.store(true, Ordering::SeqCst), &|_| {});
                    let _ = done.send(());
                });
            }
            if plug % 3 == 0 {
                let started = Instant::now();
                while !connected.load(Ordering::SeqCst) && started.elapsed() < Duration::from_secs(5) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                std::thread::sleep(Duration::from_millis(30));
                pulled.store(true, Ordering::SeqCst);
            }
            assert_eq!(over.recv_timeout(Duration::from_secs(5)), Ok(()), "plug {plug}: its thread still there");
            assert!(connected.load(Ordering::SeqCst), "plug {plug}: no session tried");
            assert!(!claimed.load(Ordering::SeqCst), "plug {plug}: its accessory still claimed");
        }
        for session in std::mem::take(&mut *sessions.lock().unwrap()) {
            session.join().unwrap();
        }
        // Each bridge's closures let go of as its threads end: none left.
        let started = Instant::now();
        while Arc::strong_count(&alive) > 1 && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(Arc::strong_count(&alive), 1, "bridges' threads left");
    }

    #[test]
    fn an_accessory_whose_thread_is_not_made_is_tried_again_less_and_less() {
        // Its thread not made (the system out of them): tried again at a
        // look AGAIN on, then twice as long after each failure in a row;
        // made at last, not tried again while it stays plugged in. (Never
        // tried again until it was plugged anew: no session by cable.)
        let watcher = Watcher::new();
        let accessory = accessory_at(None);
        let connect: Arc<dyn Fn(UnixStream, &Device) -> Result<String, String> + Send + Sync> = Arc::new(|_, _| Err("no".into()));
        let (tries, fails) = (std::cell::Cell::new(0), std::cell::Cell::new(true));
        let spawn = |_: Box<dyn FnOnce() + Send>| -> io::Result<()> {
            tries.set(tries.get() + 1);
            if fails.get() { Err(io::Error::from_raw_os_error(libc::EAGAIN)) } else { Ok(()) }
        };
        let look = || watcher.look_at(vec![accessory.clone()], &|_| Ok(()), &|_| false, &|_| {}, connect.clone(), &spawn);
        look();
        look();
        assert_eq!(tries.get(), 1, "tried again before AGAIN");
        std::thread::sleep(AGAIN + Duration::from_millis(50));
        look();
        look();
        assert_eq!(tries.get(), 2, "not tried again AGAIN on, or more than once");
        std::thread::sleep(AGAIN + Duration::from_millis(50));
        look();
        assert_eq!(tries.get(), 2, "tried again before twice AGAIN");
        fails.set(false);
        std::thread::sleep(AGAIN);
        look();
        assert_eq!(tries.get(), 3, "not tried again twice AGAIN on");
        look();
        assert_eq!(tries.get(), 3, "made, and tried again while plugged in");
        watcher.look_at(Vec::new(), &|_| Ok(()), &|_| false, &|_| {}, connect.clone(), &spawn);
        assert!(watcher.unspawned.lock().unwrap().is_empty() && watcher.bridged.lock().unwrap().is_empty(), "kept, gone");
    }
}
