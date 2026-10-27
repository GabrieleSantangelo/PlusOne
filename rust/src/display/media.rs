//! The H.264 encoders the desktop is encoded with, and the keyframes they
//! give on request (media.py).

use gst::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A keyframe of a page of text is 80-140 KB, sent as 60-100 packets back to
/// back. Kept smaller and rarer: the app asks for one when it lost data.
pub const KEYFRAME_SECONDS: u32 = 10;  // periodic keyframe interval
pub const VBV_FRAMES: u32 = 4;         // a keyframe may be this many average frames

/// (name, GStreamer element, settings, raw format it takes), in preference
/// order. Intel's low-power encoder first, for the battery: measured at
/// 1920x1200@60, 3.8 ms to its output and 3.5% of a core, against NVENC's
/// 4.7 ms, 7% and the NVIDIA GPU woken (about 8 W more).
pub const ENCODERS: [(&str, &str, &str, &str); 5] = [
    ("va-lp", "vah264lpenc", "rate-control=cbr bitrate={bitrate} key-int-max={gop} b-frames=0 \
      ref-frames=1 target-usage=7 aud=false", "NV12"),
    ("nvenc", "nvh264enc", "preset=low-latency-hq zerolatency=true bframes=0 rc-mode=cbr \
      bitrate={bitrate} gop-size={gop} aud=false", "BGRA"),
    ("va", "vah264enc", "rate-control=cbr bitrate={bitrate} key-int-max={gop} b-frames=0 target-usage=7", "NV12"),
    ("vaapi", "vaapih264enc", "rate-control=cbr bitrate={bitrate} keyframe-period={gop} max-bframes=0 \
      tune=low-power", "NV12"),
    ("x264", "x264enc", "tune=zerolatency speed-preset=ultrafast bitrate={bitrate} key-int-max={gop} bframes=0", "I420"),
];
/// Mutter hands out BGRA as readily as BGRx; asking for it makes the colour
/// conversion a no-op for NVENC (it cost ~2.3 ms of CPU per frame).
pub const CAPTURE_FORMAT: &str = "BGRA";
/// VA's raw format on the GPU (vapostproc; on the CPU it was +30% of a core).
const VA_CONVERT: &str = "vapostproc ! video/x-raw(memory:VAMemory),format=NV12";

pub struct Encoder {
    pub name: &'static str,
    pub element: &'static str,
    settings: &'static str,
    raw: &'static str,
}

impl Encoder {
    /// The element's launch line at this bitrate and frame rate.
    pub fn launch(&self, bitrate: u32, fps: u32) -> String {
        let settings = self.settings.replace("{bitrate}", &bitrate.to_string())
            .replace("{gop}", &(fps * KEYFRAME_SECONDS).to_string());
        format!("{} name=enc {}", self.element, settings.split_whitespace().collect::<Vec<_>>().join(" "))
    }

    /// How the captured frames get to its raw format.
    pub fn convert(&self) -> String {
        match self.name {
            "va-lp" | "va" => VA_CONVERT.to_owned(),
            _ => format!("videoconvert n-threads=4 ! video/x-raw,format={}", self.raw),
        }
    }
}

/// The encoder to use: preferred if there, else the first available ("auto").
pub fn pick_encoder(preferred: &str) -> Option<Encoder> {
    ENCODERS.iter()
        .filter(|(name, ..)| preferred == "auto" || *name == preferred)
        .find(|(_, element, ..)| gst::ElementFactory::find(element).is_some())
        .map(|&(name, element, settings, raw)| Encoder { name, element, settings, raw })
}

/// Each encoder names its rate-control buffer differently; set the ones this
/// one has (kbit or milliseconds).
pub fn cap_keyframes(enc: &gst::Element, bitrate: u32, fps: u32) {
    let kbit = bitrate * VBV_FRAMES / fps;
    let ms = 1000 * VBV_FRAMES / fps;
    for (property, value) in [("vbv-buffer-size", kbit), ("cpb-size", kbit), ("cpb-length", ms), ("vbv-buf-capacity", ms)] {
        if let Some(spec) = enc.find_property(property) {
            let value = if spec.value_type() == glib::Type::I32 { (value as i32).to_value() } else { value.to_value() };
            enc.set_property_from_value(property, &value);
        }
    }
}

/// Keyframes on request that a decoder can start from (an IDR, its SPS and
/// PPS before it). GStreamer 1.24's VA encoders answer a force-key-unit
/// event with an I frame that is not an IDR, without the SPS, and only the
/// first time; flushing them starts their GOP again, with an IDR, at the
/// next frame. Done on the streaming thread, just before a frame goes in.
/// NVENC ignores the flush and honours the event; the others get the event.
pub struct Keyframes {
    pad: gst::Pad,
    flush: bool,
    wanted: Arc<AtomicBool>,
    probe: Option<gst::PadProbeId>,
}

impl Keyframes {
    /// on_flush(): on the streaming thread, after a flush threw away the
    /// frames the encoder held.
    pub fn new(enc: &gst::Element, on_flush: impl Fn() + Send + Sync + 'static) -> Keyframes {
        let pad = enc.static_pad("sink").expect("an encoder has a sink pad");
        let factory = enc.factory().map(|f| f.name().to_string()).unwrap_or_default();
        let flush = factory == "vah264lpenc" || factory == "vah264enc";
        let wanted = Arc::new(AtomicBool::new(false));
        let probe = flush.then(|| {
            let wanted = wanted.clone();
            pad.add_probe(gst::PadProbeType::BUFFER, move |pad, _info| {
                if wanted.swap(false, Ordering::Relaxed) {
                    let segment = pad.sticky_event::<gst::event::Segment>(0);
                    pad.send_event(gst::event::FlushStart::new());
                    pad.send_event(gst::event::FlushStop::new(false));  // the running time goes on
                    if let Some(segment) = segment {
                        pad.send_event(segment);  // the flush took it
                    }
                    on_flush();
                }
                gst::PadProbeReturn::Ok
            })
        }).flatten();
        Keyframes { pad, flush, wanted, probe }
    }

    pub fn request(&self) {
        if self.flush {
            self.wanted.store(true, Ordering::Relaxed);
            return;
        }
        let s = gst::Structure::builder("GstForceKeyUnit").field("all-headers", true).build();
        self.pad.send_event(gst::event::CustomDownstream::new(s));
    }

    pub fn remove(&mut self) {
        if let Some(probe) = self.probe.take() {
            self.pad.remove_probe(probe);
        }
    }
}
