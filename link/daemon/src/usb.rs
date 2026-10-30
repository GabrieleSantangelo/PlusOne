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
//! request: the user's "use the cable").

use plusone_link::log;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const SYSFS: &str = "/sys/bus/usb/devices";
const GOOGLE: &str = "18d1";
const ACCESSORY: [&str; 2] = ["2d00", "2d01"];  // accessory; accessory and adb
const SAMSUNG: &str = "04e8";
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
/// The app's READY waited for with no timeout (usbfs's 0): nothing wakes
/// meanwhile, however long no app answers; the cable pulled ends the read
/// (usbfs kills the transfer: an error).
const READY_WAIT: u32 = 0;
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
}

impl Device {
    fn node(&self) -> String {
        format!("/dev/bus/usb/{:03}/{:03}", self.bus, self.number)
    }

    pub fn is_accessory(&self) -> bool {
        self.vendor == GOOGLE && ACCESSORY.contains(&self.product.as_str())
    }

    /// An Android device: Samsung's, or one with an adb or MTP/PTP interface.
    pub fn is_android(&self) -> bool {
        if self.is_accessory() {
            return false;
        }
        self.vendor == SAMSUNG || interfaces(&self.path).iter().any(|i| {
            let class = read(&i.join("bInterfaceClass"));
            let subclass = read(&i.join("bInterfaceSubClass"));
            class == "06" || (class == "ff" && subclass == "42")
        })
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
                            serial: read(&path.join("serial")), bus, number, path });
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

/// The accessory's byte stream, bridged to a socket for a session.
pub fn open_accessory(device: &Device) -> io::Result<std::os::unix::net::UnixStream> {
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
    let fd = std::sync::Arc::new(device.open()?);
    let mut number: libc::c_uint = 0;
    if unsafe { libc::ioctl(fd.as_raw_fd(), USBDEVFS_CLAIMINTERFACE, &mut number) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let bulk = |fd: &OwnedFd, endpoint: u32, data: *mut u8, length: usize, timeout: u32| -> io::Result<usize> {
        let mut transfer = Bulk { endpoint, length: length as u32, timeout, data: data as *mut libc::c_void };
        let done = unsafe { libc::ioctl(fd.as_raw_fd(), USBDEVFS_BULK, &mut transfer) };
        if done < 0 { Err(io::Error::last_os_error()) } else { Ok(done as usize) }
    };
    let number = ready(|buf, timeout| bulk(&fd, inward, buf.as_mut_ptr(), buf.len(), timeout))?;
    let mut go = plusone_link::stream::USB_GO.to_vec();
    go.extend_from_slice(&number);
    bulk(&fd, outward, go.as_mut_ptr(), go.len(), 2000)?;
    let reader = fd.clone();
    // Its READYs sent before it saw the GO: dropped, until the session's first bytes.
    let mut started = false;
    plusone_link::stream::bridge(
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
        CHUNK)
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
    pub asked: std::sync::atomic::AtomicBool,
}

impl Watcher {
    pub fn new() -> Watcher {
        Watcher { bridged: Mutex::default(), asked: std::sync::atomic::AtomicBool::new(false) }
    }

    /// Once: what is plugged in now, done; an accessory on a thread of its
    /// own (it waits for the app). remembered: serials reached by cable before.
    pub fn look(&self, remembered: &dyn Fn(&str) -> bool, switched: &dyn Fn(&Device),
                connect: std::sync::Arc<dyn Fn(std::os::unix::net::UnixStream, &Device) -> Result<String, String> + Send + Sync>) {
        let asked = self.asked.load(std::sync::atomic::Ordering::Relaxed);
        for device in devices() {
            if device.is_accessory() {
                let key = (device.bus, device.number);
                if !self.bridged.lock().unwrap().insert(key) {
                    continue;
                }
                let connect = connect.clone();
                // Sessions on it, one after the other, as long as it is plugged in.
                let _ = std::thread::Builder::new().name("link-usb-accessory".into()).spawn(move || {
                    while devices().iter().any(|d| (d.bus, d.number) == (device.bus, device.number)) {
                        match open_accessory(&device).map_err(|e| e.to_string()).and_then(|stream| connect(stream, &device)) {
                            Ok(id) => log!("usb: a session with {} by cable, over", &id[..8.min(id.len())]),
                            Err(e) => log!("usb: the accessory at {}: {e}", device.node()),
                        }
                        std::thread::sleep(Duration::from_secs(1));
                    }
                });
            } else if device.is_android() && (asked || (!device.serial.is_empty() && remembered(&device.serial))) {
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
        let present: HashSet<(u32, u32)> = devices().iter().map(|d| (d.bus, d.number)).collect();
        self.bridged.lock().unwrap().retain(|key| present.contains(key));
    }
}

/// How often the bus is looked at.
pub const EVERY: Duration = Duration::from_secs(2);

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

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
}
