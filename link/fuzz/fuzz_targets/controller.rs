//! The congestion controller fed timings no network should give
//! (cc.rs's fuzz): never a panic, its numbers in their bounds.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| plusone_link::fuzz_controller(data));
