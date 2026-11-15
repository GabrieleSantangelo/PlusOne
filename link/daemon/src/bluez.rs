//! BlueZ, for what a tablet advertises over Bluetooth LE (ble.rs): a scan
//! for a few seconds while a tablet is looked for, and the manufacturer data
//! BlueZ keeps of each device seen, read through `busctl` (systemd's, there
//! wherever BlueZ is) and `bluetoothctl` (BlueZ's own): no library, and
//! nothing while no tablet is looked for. A scan lasts only as long as the
//! connection that asked for it (BlueZ ends it when that goes), so
//! `bluetoothctl` holds it, for as long as the window.

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the devices seen are read while scanning.
const POLL: Duration = Duration::from_millis(600);
/// A call of `busctl` that takes longer is given up (BlueZ stuck).
const CALL: Duration = Duration::from_secs(3);

/// A value of D-Bus, as `busctl` prints it (what is read of it: others are numbers).
#[derive(Debug, Clone, PartialEq)]
enum Value {
    Text(String),
    Number(u64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Pairs(Vec<(Value, Value)>),
}

/// The tokens `busctl call` prints: words, and strings in quotes (with \" and \\).
fn tokens(text: &str) -> Vec<String> {
    let (mut tokens, mut chars) = (Vec::new(), text.chars().peekable());
    while let Some(&first) = chars.peek() {
        if first.is_whitespace() {
            chars.next();
        } else if first == '"' {
            chars.next();
            let mut string = String::from('"');
            while let Some(c) = chars.next() {
                match c {
                    '\\' => string.extend(chars.next()),
                    '"' => break,
                    c => string.push(c),
                }
            }
            tokens.push(string);
        } else {
            let mut word = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                word.push(c);
                chars.next();
            }
            tokens.push(word);
        }
    }
    tokens
}

/// The first complete type of a signature, and what follows it.
fn split_type(signature: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    for (i, c) in signature.char_indices() {
        match c {
            'a' => continue,
            '(' | '{' => depth += 1,
            ')' | '}' => depth = depth.checked_sub(1)?,
            _ => {}
        }
        if depth == 0 {
            return Some(signature.split_at(i + 1));
        }
    }
    None
}

/// One value of type `signature`, from the tokens.
fn value(signature: &str, tokens: &mut impl Iterator<Item = String>, depth: usize) -> Option<Value> {
    if depth > 12 {
        return None;
    }
    match signature.chars().next()? {
        's' | 'o' | 'g' => Some(Value::Text(tokens.next()?.strip_prefix('"')?.to_owned())),
        'b' => Some(Value::Number((tokens.next()? == "true") as u64)),
        'y' | 'n' | 'q' | 'i' | 'u' | 'x' | 't' | 'h' => {
            let number = tokens.next()?;
            // Signed ones as they are (an RSSI is -60): not a number to compare, only to get past.
            Some(Value::Number(number.parse::<u64>().or_else(|_| number.parse::<i64>().map(|n| n as u64)).ok()?))
        }
        'd' => {
            tokens.next()?;
            Some(Value::Number(0))
        }
        'v' => {
            let inner = tokens.next()?;
            value(&inner, tokens, depth + 1)
        }
        'a' => {
            let count: usize = tokens.next()?.parse().ok()?;
            let element = &signature[1..];
            if element == "y" {
                let bytes: Option<Vec<u8>> = (0..count).map(|_| tokens.next()?.parse().ok()).collect();
                return Some(Value::Bytes(bytes?));
            }
            if let Some(entry) = element.strip_prefix('{').and_then(|e| e.strip_suffix('}')) {
                let (key, rest) = split_type(entry)?;
                let mut pairs = Vec::new();
                for _ in 0..count {
                    pairs.push((value(key, tokens, depth + 1)?, value(rest, tokens, depth + 1)?));
                }
                return Some(Value::Pairs(pairs));
            }
            let mut list = Vec::new();
            for _ in 0..count {
                list.push(value(element, tokens, depth + 1)?);
            }
            Some(Value::List(list))
        }
        '(' => {
            let mut inner = signature.strip_prefix('(')?.strip_suffix(')')?;
            let mut list = Vec::new();
            while !inner.is_empty() {
                let (one, rest) = split_type(inner)?;
                list.push(value(one, tokens, depth + 1)?);
                inner = rest;
            }
            Some(Value::List(list))
        }
        _ => None,
    }
}

/// A device BlueZ has seen, and what it advertised.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Device {
    pub address: String,
    /// (company identifier, data) of its manufacturer data.
    pub manufacturer: Vec<(u16, Vec<u8>)>,
    /// (UUID, data) of its service data.
    pub service: Vec<(String, Vec<u8>)>,
}

/// The devices of `busctl call org.bluez / org.freedesktop.DBus.ObjectManager
/// GetManagedObjects`'s output, with what they advertised.
pub fn devices(output: &str) -> Vec<Device> {
    let mut tokens = tokens(output).into_iter();
    let Some(signature) = tokens.next() else { return Vec::new() };
    if signature != "a{oa{sa{sv}}}" {
        return Vec::new();
    }
    let Some(Value::Pairs(objects)) = value(&signature, &mut tokens, 0) else { return Vec::new() };
    let mut found = Vec::new();
    for (_, interfaces) in objects {
        let Value::Pairs(interfaces) = interfaces else { continue };
        for (name, properties) in interfaces {
            let (Value::Text(name), Value::Pairs(properties)) = (name, properties) else { continue };
            if name != "org.bluez.Device1" {
                continue;
            }
            let mut device = Device::default();
            for (property, data) in properties {
                let Value::Text(property) = property else { continue };
                match (property.as_str(), data) {
                    ("Address", Value::Text(address)) => device.address = address,
                    ("ManufacturerData", Value::Pairs(companies)) => {
                        for (id, bytes) in companies {
                            if let (Value::Number(id), Value::Bytes(bytes)) = (id, bytes) {
                                device.manufacturer.push((id as u16, bytes));
                            }
                        }
                    }
                    ("ServiceData", Value::Pairs(services)) => {
                        for (uuid, bytes) in services {
                            if let (Value::Text(uuid), Value::Bytes(bytes)) = (uuid, bytes) {
                                device.service.push((uuid, bytes));
                            }
                        }
                    }
                    _ => {}
                }
            }
            found.push(device);
        }
    }
    found
}

/// The manufacturer data of company that the devices carry (what follows
/// the company identifier), each once.
pub fn adverts(output: &str, company: u16) -> Vec<Vec<u8>> {
    let found: BTreeSet<Vec<u8>> = devices(output).into_iter()
        .flat_map(|d| d.manufacturer.into_iter().filter(|(id, _)| *id == company).map(|(_, bytes)| bytes)).collect();
    found.into_iter().collect()
}

/// A command's output, or None if it fails or takes longer than within.
fn output(program: &str, args: &[&str], within: Duration) -> Option<String> {
    let mut child = Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null())
        .spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut text).ok().map(|_| text)
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success().then(|| reader.join().ok().flatten()).flatten(),
            Ok(None) if started.elapsed() < within => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// A scan of Bluetooth LE, held until it is dropped, and the devices it has
/// heard (the addresses `bluetoothctl` tells of: BlueZ keeps what a device
/// advertised for minutes after it stops, and only what was heard in this
/// scan is of now).
struct Scan {
    child: Child,
    heard: Arc<Mutex<BTreeSet<String>>>,
}

impl Scan {
    fn start(window: Duration) -> Option<Scan> {
        let seconds = window.as_secs().max(1).to_string();
        let mut child = Command::new("bluetoothctl").args(["--timeout", &seconds, "scan", "le"])
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
        let heard: Arc<Mutex<BTreeSet<String>>> = Arc::default();
        if let Some(stdout) = child.stdout.take() {
            let heard = heard.clone();
            let _ = std::thread::Builder::new().name("link-ble-scan".into()).spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(stdout).split(b'\n').map_while(Result::ok) {
                    if let Some(address) = address_in(&String::from_utf8_lossy(&line)) {
                        heard.lock().unwrap().insert(address);
                    }
                }
            });
        }
        Some(Scan { child, heard })
    }

    fn heard(&self) -> BTreeSet<String> {
        self.heard.lock().unwrap().clone()
    }

    /// Ended already (no adapter, or Bluetooth off: it says so and goes).
    fn ended(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The address a line of `bluetoothctl`'s tells of a device ("[CHG] Device
/// AA:BB:CC:DD:EE:FF RSSI: ..."; colours around it).
fn address_in(line: &str) -> Option<String> {
    let after = &line[line.find("Device ")? + 7..];
    let address = after.get(..17)?;
    let valid = address.bytes().enumerate().all(|(i, b)| if i % 3 == 2 { b == b':' } else { b.is_ascii_hexdigit() });
    valid.then(|| address.to_ascii_uppercase())
}

/// Scans for up to window, and gives the first thing `wanted` makes of the
/// manufacturer data of company that a device seen carries: None if there is
/// no BlueZ, no adapter on, or nothing wanted came.
pub fn scan<T>(window: Duration, company: u16, wanted: impl Fn(&[u8]) -> Option<T>) -> Option<T> {
    let mut scan = Scan::start(window)?;
    let started = Instant::now();
    while started.elapsed() < window {
        std::thread::sleep(POLL);
        let seen = output("busctl", &["--system", "call", "org.bluez", "/", "org.freedesktop.DBus.ObjectManager",
                                      "GetManagedObjects"], CALL)?;
        if let Some(found) = adverts(&seen, company).iter().find_map(|data| wanted(data)) {
            return Some(found);
        }
        if scan.ended() {
            return None;
        }
    }
    None
}

/// A tablet whose user holds pairing open, as it advertises (ble.rs).
#[derive(Debug, Clone, PartialEq)]
pub struct Tablet {
    /// The start of its app's fingerprint, 16 hex digits.
    pub id: String,
    /// What it calls itself ("" before its scan response came).
    pub name: String,
    pub address: std::net::SocketAddr,
}

/// What a device advertises, if it is a tablet holding pairing open. The name
/// is what any device nearby says: kept to what can be shown.
fn tablet(device: &Device) -> Option<Tablet> {
    let (id, address) = device.manufacturer.iter().filter(|(company, _)| *company == plusone_link::ble::COMPANY)
        .find_map(|(_, data)| plusone_link::ble::read_pairing(data))?;
    let name = device.service.iter().find(|(uuid, _)| uuid == plusone_link::ble::NAME_UUID)
        .map(|(_, bytes)| String::from_utf8_lossy(bytes).chars().filter(|c| !c.is_control()).take(80).collect::<String>())
        .unwrap_or_default();
    Some(Tablet { id, name: name.trim().to_owned(), address })
}

/// The tablets heard holding pairing open within window: scanned that long
/// (what a device advertised before the scan is not of now). None if
/// there is no BlueZ, or no adapter on.
pub fn pairing(window: Duration) -> Vec<Tablet> {
    let Some(mut scan) = Scan::start(window) else { return Vec::new() };
    let started = Instant::now();
    let mut found: BTreeMap<String, Tablet> = BTreeMap::new();
    while started.elapsed() < window {
        std::thread::sleep(POLL);
        let Some(seen) = output("busctl", &["--system", "call", "org.bluez", "/", "org.freedesktop.DBus.ObjectManager",
                                            "GetManagedObjects"], CALL) else { break };
        let heard = scan.heard();
        for device in devices(&seen).iter().filter(|d| heard.contains(&d.address.to_ascii_uppercase())) {
            if let Some(tablet) = tablet(device) {
                found.insert(tablet.id.clone(), tablet);
            }
        }
        if scan.ended() {
            break;
        }
    }
    found.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As busctl printed it (BlueZ 5.72, systemd 255): an adapter, a device that
    /// advertises manufacturer data of two companies, one with none, and a string with a quote.
    const OUTPUT: &str = r#"a{oa{sa{sv}}} 3 "/org/bluez" 1 "org.bluez.AgentManager1" 0 "/org/bluez/hci0" 2 "org.bluez.Adapter1" 3 "Address" s "B0:DC:EF:A6:04:86" "Powered" b true "UUIDs" as 2 "00001133-0000-1000-8000-00805f9b34fb" "0000110e-0000-1000-8000-00805f9b34fb" "org.bluez.Device1" 0 "/org/bluez/hci0/dev_54_DD_4F_07_78_77" 2 "org.bluez.Device1" 4 "Address" s "54:DD:4F:07:78:77" "Name" s "Tab \"S10\"" "RSSI" n -60 "ManufacturerData" a{qv} 2 117 ay 3 80 191 87 65535 ay 13 1 7 233 128 206 223 177 116 43 83 224 8 202 "org.freedesktop.DBus.Properties" 0 "/org/bluez/hci0/dev_88_8E_68_1C_EF_6A" 1 "org.bluez.Device1" 2 "Address" s "88:8E:68:1C:EF:6A" "ManufacturerData" a{qv} 0"#;

    #[test]
    fn the_manufacturer_data_of_a_company_is_read_out_of_what_busctl_prints() {
        assert_eq!(adverts(OUTPUT, 65535), vec![vec![1, 7, 233, 128, 206, 223, 177, 116, 43, 83, 224, 8, 202]]);
        assert_eq!(adverts(OUTPUT, 117), vec![vec![80, 191, 87]]);
        assert!(adverts(OUTPUT, 76).is_empty());
    }

    #[test]
    fn what_is_not_that_is_nothing() {
        for text in ["", "a{oa{sa{sv}}}", "a{oa{sa{sv}}} 1", "s \"x\"", "a{oa{sa{sv}}} 1 \"/o\" 1 \"org.bluez.Device1\" 1 \"ManufacturerData\" a{qv} 1 65535 ay 9 1 2"] {
            assert!(adverts(text, 65535).is_empty(), "{text:?}");
        }
        // A count that promises more than there is, or deeper than any signature is.
        assert!(value("a{oa{sa{sv}}}", &mut tokens("99999999999999999999").into_iter(), 0).is_none());
        let deep = "a".repeat(40) + "y";
        assert!(value(&deep, &mut "1 ".repeat(40).split(' ').map(str::to_owned), 0).is_none());
    }

    #[test]
    fn strings_keep_their_quotes_and_spaces() {
        assert_eq!(tokens(r#"s "a b \"c\" \\" 5"#), vec!["s".to_owned(), "\"a b \"c\" \\".to_owned(), "5".to_owned()]);
    }

    #[test]
    fn a_missing_program_scans_nothing() {
        // No bluetoothctl in the build's container (nor a BlueZ on a PC without Bluetooth).
        if Command::new("bluetoothctl").arg("--version").output().is_err() {
            assert_eq!(scan(Duration::from_secs(1), 65535, |data| Some(data.to_vec())), None);
        }
    }

    #[test]
    fn a_device_is_read_with_its_address_and_what_it_advertised() {
        let text = r#"a{oa{sa{sv}}} 1 "/org/bluez/hci0/dev_AA" 1 "org.bluez.Device1" 3 "Address" s "AA:BB:CC:DD:EE:FF" "ManufacturerData" a{qv} 1 65535 ay 2 9 8 "ServiceData" a{sv} 1 "0000ffff-0000-1000-8000-00805f9b34fb" ay 3 84 97 98"#;
        assert_eq!(devices(text), vec![Device {
            address: "AA:BB:CC:DD:EE:FF".into(),
            manufacturer: vec![(65535, vec![9, 8])],
            service: vec![("0000ffff-0000-1000-8000-00805f9b34fb".into(), b"Tab".to_vec())],
        }]);
    }

    #[test]
    fn a_tablet_holding_pairing_open_is_read_from_what_it_advertised() {
        let data = plusone_link::ble::pairing_data(&"ab".repeat(32), ("192.168.1.112".parse().unwrap(), 47101)).unwrap();
        let mut device = Device {
            address: "AA:BB:CC:DD:EE:FF".into(),
            manufacturer: vec![(76, vec![1, 2]), (65535, data)],
            service: vec![(plusone_link::ble::NAME_UUID.into(), "  Tab S10\n\u{7}FE+ ".as_bytes().to_vec())],
        };
        assert_eq!(tablet(&device), Some(Tablet { id: "abababababababab".into(), name: "Tab S10FE+".into(),
                                                  address: "192.168.1.112:47101".parse().unwrap() }));
        device.service.clear();
        assert_eq!(tablet(&device).map(|t| t.name), Some(String::new()), "before the scan response came");
        device.manufacturer.truncate(1);
        assert_eq!(tablet(&device), None, "not Plus One's");
        let long = "x".repeat(500);
        device.manufacturer = vec![(65535, plusone_link::ble::pairing_data(&"ab".repeat(32), ("10.0.0.2".parse().unwrap(), 1)).unwrap())];
        device.service = vec![(plusone_link::ble::NAME_UUID.into(), long.into_bytes())];
        assert_eq!(tablet(&device).unwrap().name.len(), 80);
    }

    #[test]
    fn the_address_of_a_device_is_read_out_of_a_line_of_bluetoothctl() {
        for (line, wanted) in [("\x1b[0;93m[CHG]\x1b[0m Device 3F:9E:D1:4F:A0:20 RSSI: 0xffffffdb (-37)", Some("3F:9E:D1:4F:A0:20")),
                               ("[NEW] Device 3f:9e:d1:4f:a0:20 Tab", Some("3F:9E:D1:4F:A0:20")),
                               ("Discovery started", None), ("Device 3F:9E:D1", None), ("Device ZZ:9E:D1:4F:A0:20 x", None)] {
            assert_eq!(address_in(line).as_deref(), wanted, "{line:?}");
        }
    }
}
