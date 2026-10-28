//! A response on the multicast group, from anyone on the network: read
//! without a panic, and whatever it claims stays a few names long.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let found = plusone_link::mdns::read_response("_plusone-link._tcp.local", data);
    assert!(found.len() <= data.len());
});
