//! The PC's sound to the tablet's speakers as it is (pcm.py): 16-bit stereo
//! PCM at 48 kHz, no codec, in UDP packets of 5 ms sealed under a key the
//! paired link hands over. The app plays it with a plain media AudioTrack
//! (PcmPlayback.kt). Digital silence goes for a second, then nothing until
//! there is sound again: the app pauses meanwhile.

use crate::link::datagrams::Datagrams;
use crate::log;
use gst::prelude::*;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub const PCM: u8 = 7;
pub const STATS: u8 = 5;
pub const RATE: u32 = 48000;
pub const CHANNELS: u32 = 2;
pub const FRAMES: usize = 240;  // per packet: 5 ms
pub const BYTES: usize = FRAMES * CHANNELS as usize * 2;
const QUIET_AFTER: u32 = 200;   // packets of silence in a row (a second) after which none go
const STATS_EVERY: u32 = 30;    // the app's reports (one a second) logged: one in so many, and each with a loss

/// (held, missed, late, left out, behind, gaps) from an app's STATS, in
/// frames and packets; what it does not say, 0. None if it is not one.
pub fn app_report(body: &[u8]) -> Option<[u32; 6]> {
    if body.first() != Some(&STATS) {
        return None;
    }
    let n = match body.len() {
        25 => 6,
        21 => 5,
        13 => 3,
        _ => return None,
    };
    let mut out = [0u32; 6];
    for (i, value) in out.iter_mut().take(n).enumerate() {
        *value = u32::from_be_bytes(body[1 + 4 * i..5 + 4 * i].try_into().unwrap());
    }
    Some(out)
}

struct Cut {
    pending: Vec<u8>,
    quiet: u32,  // packets of silence in a row
    sent: u32,
}

pub struct PcmSender {
    pub channel: Arc<Datagrams>,
    pipeline: gst::Pipeline,
    running: Arc<AtomicBool>,
    captured: Arc<AtomicU64>,  // buffers of sound from PipeWire: it flows while this grows
    broken: Arc<Mutex<Option<String>>>,
    listener: Option<JoinHandle<()>>,
}

impl PcmSender {
    /// source: a GStreamer source (the speakers' sink's monitor, or a test tone).
    pub fn new(source: &str) -> Result<PcmSender, String> {
        let channel = Arc::new(Datagrams::new(None).map_err(|e| e.to_string())?);
        let pipeline = gst::parse::launch(&format!(
            "{source} ! audioconvert ! audioresample ! audio/x-raw,format=S16LE,rate={RATE},channels={CHANNELS},\
             layout=interleaved ! appsink name=out sync=false max-buffers=0"))
            .map_err(|e| e.to_string())?
            .downcast::<gst::Pipeline>().map_err(|_| "not a pipeline".to_owned())?;
        let captured = Arc::new(AtomicU64::new(0));
        let cut = Mutex::new(Cut { pending: Vec::new(), quiet: 0, sent: 0 });
        let sink = pipeline.by_name("out").unwrap().downcast::<gst_app::AppSink>().unwrap();
        {
            let (channel, captured) = (channel.clone(), captured.clone());
            // Streaming thread: the sound cut into 5 ms packets, out at once.
            sink.set_callbacks(gst_app::AppSinkCallbacks::builder().new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                captured.fetch_add(1, Ordering::Relaxed);
                let mut guard = cut.lock().unwrap();
                let Cut { pending, quiet, sent } = &mut *guard;
                pending.extend_from_slice(map.as_slice());
                let whole = pending.len() - pending.len() % BYTES;
                let mut bodies = Vec::new();
                for at in (0..whole).step_by(BYTES) {
                    let packet = &pending[at..at + BYTES];
                    if packet.iter().any(|&b| b != 0) {
                        *quiet = 0;
                    } else if *quiet >= QUIET_AFTER {
                        continue;
                    } else {
                        *quiet += 1;
                    }
                    let mut body = Vec::with_capacity(5 + BYTES);
                    body.push(PCM);
                    body.extend_from_slice(&sent.to_be_bytes());
                    body.extend_from_slice(packet);
                    bodies.push(body);
                    *sent = sent.wrapping_add(1);
                }
                pending.drain(..whole);
                drop(guard);
                let refs: Vec<&[u8]> = bodies.iter().map(Vec::as_slice).collect();
                let _ = channel.send_all(&refs);
                Ok(gst::FlowSuccess::Ok)
            }).build());
        }
        let broken: Arc<Mutex<Option<String>>> = Arc::default();
        {
            let broken = broken.clone();
            pipeline.bus().unwrap().set_sync_handler(move |_, message| {
                if let gst::MessageView::Error(e) = message.view() {
                    let mut broken = broken.lock().unwrap();
                    if broken.is_none() {
                        *broken = Some(format!("{}: {}", e.src().map(|s| s.name().to_string()).unwrap_or_default(), e.error()));
                    }
                }
                gst::BusSyncReply::Drop
            });
        }
        Ok(PcmSender { channel, pipeline, running: Arc::new(AtomicBool::new(true)), captured, broken, listener: None })
    }

    /// The session's key and ports, as the request's "sdp" (JSON).
    pub fn offer(&self) -> String {
        serde_json::json!({"key": crate::link::trust::hex(&self.channel.key), "port": self.channel.port(),
                           "rate": RATE, "channels": CHANNELS, "frames": FRAMES}).to_string()
    }

    /// The app's answer (its port, JSON): the sound starts.
    pub fn answer(&mut self, text: &str, peer: std::net::IpAddr) -> Result<(), String> {
        let reply: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let port = reply.get("port").and_then(Value::as_u64).filter(|p| (1..65536).contains(p))
            .ok_or("the app's answer is not one")?;
        self.channel.set_app((peer, port as u16).into());
        self.pipeline.set_state(gst::State::Playing).map_err(|e| e.to_string())?;
        let (channel, running) = (self.channel.clone(), self.running.clone());
        self.listener = Some(std::thread::spawn(move || {
            let mut reports = 0;
            while running.load(Ordering::Relaxed) {
                let Some([held, missed, late, dropped, behind, gaps]) = channel.receive(Duration::from_secs(1))
                    .and_then(|b| app_report(&b)) else { continue };
                reports += 1;
                if missed > 0 || late > 0 || gaps > 0 || reports % STATS_EVERY == 1 {
                    let ms = |f: u32| f as f64 / RATE as f64 * 1000.0;
                    log!("speakers: the tablet holds {:.0} ms{}; missed {missed} packets, {late} came late{}{}",
                         ms(held), if behind > 0 { format!(", its speaker is {:.0} ms behind", ms(behind)) } else { String::new() },
                         if dropped > 0 { format!(", {dropped} nearly silent left out") } else { String::new() },
                         if gaps > 0 { format!("; {gaps} gaps heard") } else { String::new() });
                }
            }
        }));
        Ok(())
    }

    /// Buffers captured: the sound goes as long as this grows (silence too).
    pub fn captured(&self) -> u64 {
        self.captured.load(Ordering::Relaxed)
    }

    pub fn failed(&self) -> Option<String> {
        self.broken.lock().unwrap().clone()
    }

    pub fn close(&mut self) {
        if !self.running.swap(false, Ordering::Relaxed) {
            return;
        }
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

impl Drop for PcmSender {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_s_reports_of_each_version_are_read() {
        let mut report = vec![STATS];
        for v in [1920u32, 2, 3, 4, 7000, 1] {
            report.extend_from_slice(&v.to_be_bytes());
        }
        assert_eq!(app_report(&report), Some([1920, 2, 3, 4, 7000, 1]));
        assert_eq!(app_report(&report[..21]), Some([1920, 2, 3, 4, 7000, 0]));
        assert_eq!(app_report(&report[..13]), Some([1920, 2, 3, 0, 0, 0]));
        assert_eq!(app_report(&report[..17]), None);
    }
}
