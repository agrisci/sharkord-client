//! Windows: capture (DXGI, `d3d11screencapturesrc`) and encode (AMF) with GStreamer, from the
//! runtime subset `scripts/stage-native.js` bundles. Measured on an RX 9060 XT: DXGI capture
//! straight into `d3d11convert` and AMF with no copies, `videorate` holding the declared rate
//! the encoder budgets against, a one-frame leaky queue so a busy encoder never holds the
//! source back, a keyframe a minute (more on request), no B-frames, and AMF AV1's keyframe
//! poke (it ignores force-key-unit).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;
use serde_json::{Value, json};

use crate::{Codec, Out, Start, event, fit, h264_sps, write_record};

/// One monitor as GStreamer's d3d11 device provider reports it.
#[cfg(windows)]
struct Monitor {
    handle: u64,
    label: String,
    primary: bool,
    size: (u32, u32),
}

/// The d3d11 provider's monitors. Only `device.api == d3d11`: the d3d12 provider answers the
/// same class and lists every screen a second time, with handles for the wrong element.
#[cfg(windows)]
fn monitors() -> Result<Vec<Monitor>> {
    let monitor = gst::DeviceMonitor::new();
    monitor
        .add_filter(Some("Source/Monitor"), None)
        .ok_or_else(|| anyhow!("no device provider matched Source/Monitor"))?;
    monitor.start().map_err(|_| anyhow!("could not start the screen device monitor"))?;
    let found = monitor
        .devices()
        .iter()
        .filter_map(|device| {
            let p = device.properties()?;
            if p.get::<String>("device.api").ok().as_deref() != Some("d3d11") {
                return None;
            }
            let edge = |n: &str| p.get::<i32>(n).ok();
            // Physical pixels (what the element captures), not the DPI-scaled desktop rect
            let size = (
                u32::try_from(edge("display.coordinates.right")? - edge("display.coordinates.left")?).ok()?,
                u32::try_from(edge("display.coordinates.bottom")? - edge("display.coordinates.top")?).ok()?,
            );
            Some(Monitor {
                handle: p.get::<u64>("device.hmonitor").ok()?,
                label: device.display_name().to_string(),
                primary: p.get::<bool>("device.primary").unwrap_or(false),
                size,
            })
        })
        .collect();
    monitor.stop();
    Ok(found)
}

/// The monitor the app picked: by label (Electron's `Display.label` and the provider's
/// display name are both Windows' friendly name), else the primary one.
#[cfg(windows)]
fn choose(monitors: Vec<Monitor>, start: &Start) -> Option<Monitor> {
    let mut monitors = monitors;
    if let Some(label) = &start.label
        && let Some(i) = monitors.iter().position(|m| &m.label == label && m.primary == start.primary)
            .or_else(|| monitors.iter().position(|m| &m.label == label))
    {
        return Some(monitors.swap_remove(i));
    }
    let i = monitors.iter().position(|m| m.primary)?;
    Some(monitors.swap_remove(i))
}

/// The target bitrate, and AMF's peak with it
fn set_rate(encoder: &gst::Element, kbps: u32) -> Result<()> {
    let kbps = kbps.max(100);
    let _ = set_int(encoder, "max-bitrate", kbps);
    set_int(encoder, "bitrate", kbps)
}

/// Writes an integer property whichever integer type the element declares it with
/// (AMF's `gop-size` is signed on H.264 and unsigned on AV1), clamped to its range. Skips a
/// missing or read-only property rather than panicking.
fn set_int(element: &gst::Element, name: &str, value: u32) -> Result<()> {
    let pspec = element.find_property(name).ok_or_else(|| anyhow!("{name}: no such property"))?;
    if !pspec.flags().contains(gst::glib::ParamFlags::WRITABLE) {
        bail!("{name}: not writable");
    }
    let value = match (pspec.downcast_ref::<gst::glib::ParamSpecUInt>(), pspec.downcast_ref::<gst::glib::ParamSpecInt>()) {
        (Some(p), _) => value.clamp(p.minimum(), p.maximum()),
        (_, Some(p)) => value.clamp(p.minimum().max(0) as u32, p.maximum().max(0) as u32),
        _ => value,
    };
    let t = pspec.value_type();
    if t == u32::static_type() {
        element.set_property(name, value);
    } else if t == i32::static_type() {
        element.set_property(name, i32::try_from(value)?);
    } else if t == u64::static_type() {
        element.set_property(name, u64::from(value));
    } else if t == i64::static_type() {
        element.set_property(name, i64::from(value));
    } else {
        bail!("{name}: is {t}, not an integer");
    }
    Ok(())
}

fn make(factory: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory).build().with_context(|| format!("{factory} unavailable"))
}

/// The hardware encoder for `codec`: the first factory that builds. AMF registers one per GPU
/// (`amfh264device1enc`).
fn encoder(codec: Codec) -> Result<gst::Element> {
    let c = match codec {
        Codec::H264 => "h264",
        Codec::Av1 => "av1",
    };
    std::iter::once(format!("amf{c}enc"))
        .chain((1..4).map(|i| format!("amf{c}device{i}enc")))
        .find_map(|n| gst::ElementFactory::make(&n).build().ok())
        .ok_or_else(|| anyhow!("no {c} encoder (this helper needs an AMD GPU)"))
}

/// Whether `codec` can really be encoded here: an encoder builds and opens its device. A
/// registered factory alone doesn't prove it (#77).
fn can_encode(codec: Codec) -> bool {
    encoder(codec).is_ok_and(|e| {
        let ok = e.set_state(gst::State::Ready).is_ok();
        let _ = e.set_state(gst::State::Null);
        ok
    })
}

/// Where the frames come from and what memory they stay in up to the encoder.
struct Source {
    head: Vec<gst::Element>,
    /// Caps prefix from the head up to `convert`
    memory: &'static str,
    convert: &'static str,
    /// Caps prefix the encoder takes
    encoder_memory: &'static str,
    size: (u32, u32),
    label: Option<String>,
}

/// What the capture needs to stay alive: nothing on Windows (the portal session on Linux).
pub type Capture = ();

fn source(start: &Start, _capture: &Capture) -> Result<Source> {
    let monitor = choose(monitors()?, start);
    let mut src = gst::ElementFactory::make("d3d11screencapturesrc").property("show-cursor", true);
    src = match &monitor {
        Some(m) => src.property("monitor-handle", m.handle),
        None => src.property("monitor-index", -1i32),
    };
    let d3d11 = "video/x-raw(memory:D3D11Memory)";
    Ok(Source {
        head: vec![src.build().context("d3d11screencapturesrc unavailable")?],
        memory: d3d11,
        convert: "d3d11convert",
        encoder_memory: d3d11,
        size: monitor.as_ref().map_or((start.width, start.height), |m| m.size),
        label: monitor.map(|m| m.label),
    })
}

pub struct Share {
    pipeline: gst::Pipeline,
    appsink: AppSink,
    encoder: gst::Element,
    /// Its input caps show whether frames arrive in GPU memory (reported with the first frame)
    convert: gst::Element,
    /// Its counters show whether the source keeps up (in) or the encoder does (out vs sent)
    rate: gst::Element,
    codec: Codec,
    key_interval: u32,
    /// AMF AV1: keyframes by shortening gop-size (`poking` while one is on its way)
    poke: bool,
    poking: Arc<AtomicBool>,
    /// The `started` event, sent with the first frame (with the negotiated sizes)
    started: Value,
    _capture: Capture,
}

fn build(start: &Start, out: &Out, capture: Capture) -> Result<Share> {
    let source = source(start, &capture)?;
    let (width, height) = fit(source.size, (start.width, start.height));
    let fps = start.fps;
    // A keyframe a minute: viewers joining and packet loss get one on request (PLI), and a
    // scheduled 1080p keyframe (~400 KB measured on real desktop content) is a burst larger than a
    // second's budget, which kept Chromium's bandwidth estimate from ever rising
    let key_interval = fps * 60;

    let pipeline = gst::Pipeline::with_name("share");
    let caps = |s: String| -> Result<gst::Element> {
        Ok(gst::ElementFactory::make("capsfilter").property("caps", s.parse::<gst::Caps>()?).build()?)
    };
    let src_caps = caps(source.memory.to_owned())?;
    // Constant output rate: the encoder's rate control budgets against the declared rate
    let rate = make("videorate")?;
    let rate_caps = caps(format!("{},framerate={fps}/1", source.memory))?;
    // One frame, newest wins: a busy encoder drops a frame instead of stalling the source
    let queue = gst::ElementFactory::make("queue")
        .property_from_str("leaky", "downstream")
        .property("max-size-buffers", 1u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .build()?;
    let convert = make(source.convert)?;
    let enc_caps = caps(format!("{},format=NV12,width={width},height={height},framerate={fps}/1", source.encoder_memory))?;

    let encoder = encoder(start.codec)?;
    let encoder_name = encoder.factory().map(|f| f.name().to_string()).unwrap_or_default();
    // Latency-constrained VBR capped at the target, AMF's real-time mode, measured with the
    // smallest keyframes (248 KB against 408 KB by default at 1080p) and a gentle first frame
    if encoder.find_property("rate-control").is_some() {
        encoder.set_property_from_str("rate-control", "lcvbr");
    }
    set_rate(&encoder, start.kbps)?;
    if let Err(e) = set_int(&encoder, "gop-size", key_interval) {
        event(out, json!({ "type": "warning", "message": format!("keyframe interval not set: {e}") }));
    }
    if start.codec == Codec::H264 {
        let _ = set_int(&encoder, "b-frames", 0);
    }
    // The profile the SDP promises: Sharkord negotiates Constrained Baseline (42e01f) for
    // H.264, and AMF takes its profile from downstream caps (it has no property for it)
    let profile = caps(match start.codec {
        Codec::H264 => "video/x-h264,profile=constrained-baseline".to_owned(),
        Codec::Av1 => "video/x-av1".to_owned(),
    })?;
    let parser = match start.codec {
        Codec::H264 => gst::ElementFactory::make("h264parse").property("config-interval", -1i32).build()?,
        Codec::Av1 => make("av1parse")?,
    };
    let out_caps: gst::Caps = match start.codec {
        // SPS/PPS in front of every keyframe, so a viewer joining mid-stream can decode
        Codec::H264 => "video/x-h264,stream-format=byte-stream,alignment=au".parse()?,
        Codec::Av1 => "video/x-av1,stream-format=obu-stream,alignment=tu".parse()?,
    };
    let appsink = AppSink::builder().caps(&out_caps).sync(false).max_buffers(4).drop(true).build();

    let chain: Vec<&gst::Element> = source.head.iter()
        .chain([&src_caps, &rate, &rate_caps, &queue, &convert, &enc_caps, &encoder, &profile, &parser])
        .collect();
    pipeline.add_many(chain.iter().copied())?;
    pipeline.add(appsink.upcast_ref::<gst::Element>())?;
    gst::Element::link_many(chain).context("capture pipeline link failed")?;
    parser.link(&appsink).context("appsink link failed")?;

    // AMF AV1 ignores force-key-unit: shorten gop-size to 1 and restore it on the next
    // buffer out of the encoder (costs one extra keyframe)
    let poke = start.codec == Codec::Av1;
    let poking = Arc::new(AtomicBool::new(false));
    if poke && let Some(pad) = encoder.static_pad("src") {
        let poking = poking.clone();
        let weak = encoder.downgrade();
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            if poking.swap(false, Ordering::Relaxed)
                && let Some(encoder) = weak.upgrade()
            {
                let _ = set_int(&encoder, "gop-size", key_interval);
            }
            gst::PadProbeReturn::Ok
        });
    }

    let started = json!({
        "type": "started",
        "encoder": encoder_name,
        "monitor": source.label,
        "source": [source.size.0, source.size.1],
        "size": [width, height],
        "fps": fps,
    });
    Ok(Share { pipeline, appsink, encoder, convert, rate, codec: start.codec, key_interval, poke, poking, started, _capture: capture })
}

/// Elements the capture graph is built from. The encoders are not among them: they register
/// only with a supported GPU, and the packaging self-check runs on CI without one.
const REQUIRED: &[&str] = &[
    "d3d11screencapturesrc", "d3d11convert", "videorate", "capsfilter", "queue", "appsink",
    "h264parse", "av1parse",
];

/// `--check`: does this bundle load everything, and which codecs can this machine encode
pub fn check() -> Value {
    if let Err(e) = gst::init() {
        return json!({ "missing": ["gstreamer"], "h264": false, "av1": false, "error": e.to_string() });
    }
    let missing: Vec<_> = REQUIRED.iter().filter(|n| gst::ElementFactory::find(n).is_none()).collect();
    json!({ "missing": missing, "h264": can_encode(Codec::H264), "av1": can_encode(Codec::Av1) })
}

impl Share {
    pub fn start(start: &Start, out: &Out, capture: Capture) -> Result<Share> {
        gst::init()?;
        let share = build(start, out, capture)?;
        if let Err(e) = share.pipeline.set_state(gst::State::Playing) {
            let _ = share.pipeline.set_state(gst::State::Null);
            return Err(anyhow::Error::new(e).context("could not start the capture"));
        }
        Ok(share)
    }

    /// Pulls encoded frames and writes them out until `stop`
    pub fn run(&self, out: &Out, stop: &AtomicBool) -> Result<()> {
        pump(self, out, stop)
    }

    pub fn finish(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    pub fn keyframe(&self) {
        if self.poke {
            // Arm before writing: the probe runs on the streaming thread
            self.poking.store(true, Ordering::Relaxed);
            let _ = set_int(&self.encoder, "gop-size", 1);
        } else {
            self.encoder.send_event(gstreamer_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build());
        }
    }

    pub fn bitrate(&self, kbps: u32) -> Result<()> {
        set_rate(&self.encoder, kbps)
    }
}

/// Pulls encoded frames and writes them out until `stop`, reporting stats every 2 s.
fn pump(share: &Share, out: &Out, stop: &AtomicBool) -> Result<()> {
    let bus = share.pipeline.bus().context("pipeline has no bus")?;
    let (mut frames, mut keys, mut bytes, mut since) = (0u64, 0u64, 0u64, Instant::now());
    let mut keys_in_a_row = 0u32;
    let mut reported_sps = share.codec != Codec::H264;
    let mut frames_total = 0u64;
    while !stop.load(Ordering::Relaxed) {
        if let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos]) {
            match msg.view() {
                gst::MessageView::Error(e) => bail!("{} ({:?})", e.error(), e.debug()),
                _ => bail!("capture ended"),
            }
        }
        let Some(sample) = share.appsink.try_pull_sample(gst::ClockTime::from_mseconds(100)) else {
            continue;
        };
        let buffer = sample.buffer().context("sample had no buffer")?;
        let map = buffer.map_readable().context("could not map encoded buffer")?;
        let key = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
        let pts = buffer.pts().map_or(0, |t| t.useconds());
        // The AV1 poke's restore can lose a race with the next poke and leave gop-size at 1:
        // every frame a keyframe. Three in a row nobody asked for means that; put it back.
        keys_in_a_row = if key { keys_in_a_row + 1 } else { 0 };
        if keys_in_a_row >= 3 && share.poke && !share.poking.load(Ordering::Relaxed) {
            let _ = set_int(&share.encoder, "gop-size", share.key_interval);
            keys_in_a_row = 0;
        }
        if frames_total == 0 {
            // `started` with the first frame: only now are the sizes negotiated (see `build`)
            let caps = |pad| share.convert.static_pad(pad).and_then(|p| p.current_caps());
            let size = |caps: &Option<gst::Caps>| caps.as_ref().and_then(|c| {
                let s = c.structure(0)?;
                Some(json!([s.get::<i32>("width").ok()?, s.get::<i32>("height").ok()?]))
            });
            let (input, output) = (caps("sink"), caps("src"));
            let mut started = share.started.clone();
            if let Some(v) = size(&input) { started["source"] = v }
            if let Some(v) = size(&output) { started["size"] = v }
            event(out, started);
            if let Some(caps) = input {
                event(out, json!({ "type": "input", "caps": caps.to_string() }));
            }
        }
        frames_total += 1;
        if key && !reported_sps && let Some(sps) = h264_sps(&map) {
            event(out, sps);
            reported_sps = true;
        }
        write_record(out, 1, u8::from(key), pts, &map).context("stdout closed")?;
        frames += 1;
        keys += u64::from(key);
        bytes += map.len() as u64;
        let elapsed = since.elapsed();
        if elapsed >= Duration::from_secs(2) {
            let s = elapsed.as_secs_f64();
            // Totals since the start: frames captured, made constant-rate, duplicated, dropped
            let rate = ["in", "out", "duplicate", "drop"].map(|p| share.rate.property::<u64>(p));
            event(out, json!({
                "type": "stats",
                "fps": (frames as f64 / s * 10.0).round() / 10.0,
                "kbps": (bytes as f64 * 8.0 / 1000.0 / s).round(),
                "keyframes": keys,
                "rate": rate,
            }));
            (frames, keys, bytes, since) = (0, 0, 0, Instant::now());
        }
    }
    Ok(())
}
