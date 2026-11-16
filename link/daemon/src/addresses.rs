//! Where each tablet can be dialled: the addresses it was reached at, and
//! those it said itself (its "net" message, over a cable or any path: the
//! Wi-Fi's address while it is plugged in), a few for each, newest first,
//! kept as the networks come and go. Dialled only while on a network this
//! PC is on now (a home's address is no use at the office, and is there
//! again at home).

use serde_json::{json, Value};
use std::net::{IpAddr, SocketAddr};

type Network = (IpAddr, IpAddr);

/// Addresses kept for a tablet, newest first.
const KEPT: usize = 8;
/// Dialled one after the other, each up to a second and a half.
pub const TRIED: usize = 2;

/// What the state holds for a tablet: a list of "ip:port", or (as it was
/// before) one.
pub fn read(entry: Option<&Value>) -> Vec<SocketAddr> {
    match entry {
        Some(Value::String(one)) => one.parse().into_iter().collect(),
        Some(Value::Array(list)) => list.iter().filter_map(|a| a.as_str()?.parse().ok()).collect(),
        _ => Vec::new(),
    }
}

pub fn write(addresses: &[SocketAddr]) -> Value {
    json!(addresses.iter().map(|a| a.to_string()).collect::<Vec<_>>())
}

/// The addresses known after learning new ones: the new ones first, then
/// those before that are not on a network a new one is on (those are
/// stale there: a DHCP lease that moved), up to KEPT. Other networks' stay.
pub fn merge(old: &[SocketAddr], new: &[SocketAddr], networks: &[Network]) -> Vec<SocketAddr> {
    let theirs: Vec<Network> =
        networks.iter().filter(|n| new.iter().any(|a| plusone_link::sys::in_networks(a.ip(), &[**n]))).copied().collect();
    let mut merged: Vec<SocketAddr> = Vec::new();
    for address in new.iter().chain(old) {
        let stale = !new.contains(address) && plusone_link::sys::in_networks(address.ip(), &theirs);
        if !stale && !merged.contains(address) {
            merged.push(*address);
        }
    }
    merged.truncate(KEPT);
    merged
}

/// What to dial now, in order: the addresses on a network this PC is on
/// (all, if its networks cannot be told; loopback is always on one).
pub fn to_dial(known: &[SocketAddr], networks: &[Network]) -> Vec<SocketAddr> {
    known.iter().filter(|a| networks.is_empty() || a.ip().is_loopback() || plusone_link::sys::in_networks(a.ip(), networks))
        .take(TRIED).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(address: &str) -> SocketAddr {
        address.parse().unwrap()
    }

    fn net(address: &str, mask: &str) -> Network {
        (address.parse().unwrap(), mask.parse().unwrap())
    }

    #[test]
    fn a_state_from_before_holds_one_address_as_a_string() {
        assert_eq!(read(Some(&json!("192.168.1.9:47101"))), vec![at("192.168.1.9:47101")]);
        assert_eq!(read(Some(&json!(["192.168.1.9:47101", "nonsense", "[2001:db8::1]:47101"]))),
                   vec![at("192.168.1.9:47101"), at("[2001:db8::1]:47101")]);
        assert!(read(Some(&json!(["x"]))).is_empty() && read(None).is_empty() && read(Some(&json!(5))).is_empty());
        assert_eq!(read(Some(&write(&[at("10.0.0.2:1")]))), vec![at("10.0.0.2:1")]);
    }

    #[test]
    fn an_address_of_the_same_network_replaces_the_old_one() {
        let home = [net("192.168.1.155", "255.255.255.0")];
        let merged = merge(&[at("192.168.1.20:47101")], &[at("192.168.1.112:47101")], &home);
        assert_eq!(merged, vec![at("192.168.1.112:47101")]);
    }

    #[test]
    fn the_addresses_of_other_networks_stay() {
        // The office's, learnt there, kept while at home; both of the home's
        // (4 and 6) told together stay too, newest first.
        let home = [net("192.168.1.155", "255.255.255.0"), net("2001:db8:1::5", "ffff:ffff:ffff:ffff::")];
        let old = [at("192.168.1.20:47101"), at("10.151.253.132:47101")];
        let new = [at("192.168.1.112:47101"), at("[2001:db8:1::9]:47101")];
        assert_eq!(merge(&old, &new, &home),
                   vec![at("192.168.1.112:47101"), at("[2001:db8:1::9]:47101"), at("10.151.253.132:47101")]);
    }

    #[test]
    fn two_addresses_of_one_network_told_together_both_stay() {
        let home = [net("2001:db8:1::5", "ffff:ffff:ffff:ffff::")];
        let new = [at("[2001:db8:1::9]:47101"), at("[2001:db8:1::a]:47101")];
        assert_eq!(merge(&[], &new, &home), new.to_vec());
        assert_eq!(merge(&new, &new, &home), new.to_vec(), "told again: no copies");
    }

    #[test]
    fn nothing_is_dropped_when_the_networks_cannot_be_told() {
        assert_eq!(merge(&[at("10.0.0.2:1")], &[at("10.0.0.3:1")], &[]), vec![at("10.0.0.3:1"), at("10.0.0.2:1")]);
    }

    #[test]
    fn at_most_kept_are_kept() {
        let old: Vec<SocketAddr> = (0..KEPT as u8).map(|i| at(&format!("10.{i}.0.2:1"))).collect();
        let merged = merge(&old, &[at("10.99.0.2:1")], &[]);
        assert_eq!((merged.len(), merged[0], merged.contains(&old[KEPT - 1])), (KEPT, at("10.99.0.2:1"), false));
    }

    #[test]
    fn only_the_networks_we_are_on_are_dialled() {
        let home = [net("192.168.1.155", "255.255.255.0")];
        let known = [at("10.151.253.132:47101"), at("192.168.1.112:47101"), at("192.168.1.113:47101"), at("192.168.1.114:47101")];
        assert_eq!(to_dial(&known, &home), vec![at("192.168.1.112:47101"), at("192.168.1.113:47101")]);
        assert!(to_dial(&known[..1], &home).is_empty());
        assert_eq!(to_dial(&known[..1], &[]), vec![known[0]], "no telling: tried");
        assert_eq!(to_dial(&[at("127.0.0.1:47101")], &home), vec![at("127.0.0.1:47101")], "loopback is on every network");
    }
}
