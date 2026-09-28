//! Native screen capture + hardware encode for the Electron client's screen share.
//!
//! Sharkord's page keeps its own WebRTC connection; the preload swaps each outgoing
//! placeholder frame's payload for a frame from here (see `electron/preload.js`). This
//! process only captures and encodes -- no network.
//!
//! Protocol, one share per process:
//! - stdin, JSON lines: `{"cmd":"start","codec":"h264"|"av1","width","height","fps","kbps",
//!   "label","primary"}`, `{"cmd":"keyframe"}`, `{"cmd":"bitrate","kbps"}`, `{"cmd":"stop"}`.
//!   EOF means the app is gone: stop and exit.
//! - stdout, records: a 16-byte header (u8 kind, u8 flags, u16 reserved, u32 LE length,
//!   u64 LE pts in microseconds) and the payload. Kind 1 is an encoded frame (flag 1 =
//!   keyframe), kind 2 a JSON event (`started` once capturing, `stream` with the first H.264
//!   keyframe's SPS profile and level, `stats` every 2 s, `warning`, and `error` before the
//!   process gives up -- every failure after `start` sends one).
//!
//! The graph and its settings follow the native client (`../sharkord-client`), where each
//! was measured on an RX 9060 XT: DXGI capture straight into `d3d11convert` and AMF with no
//! copies, `videorate` holding the declared rate the encoder budgets against, a one-frame
//! leaky queue so a busy encoder never holds the source back, a keyframe a minute (more on
//! request), no B-frames, and AMF AV1's keyframe poke (it ignores force-key-unit).

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;
use serde_json::{Value, json};

type Out = Arc<Mutex<std::io::BufWriter<std::io::Stdout>>>;

fn write_record(out: &Out, kind: u8, flags: u8, pts_us: u64, payload: &[u8]) -> std::io::Result<()> {
    let mut header = [0u8; 16];
    header[0] = kind;
    header[1] = flags;
    header[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    header[8..16].copy_from_slice(&pts_us.to_le_bytes());
    let mut out = out.lock().expect("stdout lock poisoned");
    out.write_all(&header)?;
    out.write_all(payload)?;
    out.flush()
}

fn event(out: &Out, value: Value) {
    let _ = write_record(out, 2, 0, 0, value.to_string().as_bytes());
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Codec {
    H264,
    Av1,
}

struct Start {
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    kbps: u32,
    label: Option<String>,
    primary: bool,
}

fn parse_start(v: &Value) -> Result<Start> {
    let num = |k: &str, d: u64| v.get(k).and_then(Value::as_u64).unwrap_or(d) as u32;
    let codec = match v.get("codec").and_then(Value::as_str) {
        Some("h264") => Codec::H264,
        Some("av1") => Codec::Av1,
        other => bail!("unsupported codec {other:?}"),
    };
    Ok(Start {
        codec,
        width: num("width", 1920),
        height: num("height", 1080),
        fps: num("fps", 60).clamp(1, 240),
        kbps: num("kbps", 4000).max(100),
        label: v.get("label").and_then(Value::as_str).map(str::to_owned),
        primary: v.get("primary").and_then(Value::as_bool).unwrap_or(true),
    })
}

/// One monitor as GStreamer's d3d11 device provider reports it.
struct Monitor {
    handle: u64,
    label: String,
    primary: bool,
    size: (u32, u32),
}

/// The d3d11 provider's monitors. Only `device.api == d3d11`: the d3d12 provider answers the
/// same class and lists every screen a second time, with handles for the wrong element.
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

/// The largest even size inside `max` with the source's aspect ratio, never larger than
/// the source -- what a browser does with getDisplayMedia's width/height (copied from the
/// native client's `capture_front_end::fit`).
fn fit(source: (u32, u32), max: (u32, u32)) -> (u32, u32) {
    let ((sw, sh), (mw, mh)) = (
        (u64::from(source.0.max(1)), u64::from(source.1.max(1))),
        (u64::from(max.0), u64::from(max.1)),
    );
    let even = |v: u64| (v & !1).max(2) as u32;
    let scaled = |n: u64, d: u64| ((n + d) / (2 * d) * 2).max(2) as u32;
    if sw * mh <= sh * mw {
        let h = sh.min(mh);
        (scaled(sw * h, sh).min(even(sw.min(mw))), even(h))
    } else {
        let w = sw.min(mw);
        (even(w), scaled(sh * w, sw).min(even(sh.min(mh))))
    }
}

/// Writes an integer property whichever integer type the element declares it with
/// (AMF's `gop-size` is signed on H.264 and unsigned on AV1). Skips a missing or
/// read-only property rather than panicking.
fn set_int(element: &gst::Element, name: &str, value: u32) -> Result<()> {
    let pspec = element.find_property(name).ok_or_else(|| anyhow!("{name}: no such property"))?;
    if !pspec.flags().contains(gst::glib::ParamFlags::WRITABLE) {
        bail!("{name}: not writable");
    }
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

/// AMF registers one factory per device (`amfh264enc`, `amfh264device1enc`); take the
/// first that builds.
fn amf_encoder(codec: Codec) -> Result<gst::Element> {
    let stem = match codec {
        Codec::H264 => "amfh264",
        Codec::Av1 => "amfav1",
    };
    let mut names = vec![format!("{stem}enc")];
    names.extend((1..4).map(|i| format!("{stem}device{i}enc")));
    names
        .iter()
        .find_map(|n| gst::ElementFactory::make(n).build().ok())
        .ok_or_else(|| anyhow!("no AMF {stem} encoder (this helper needs an AMD GPU)"))
}

struct Share {
    pipeline: gst::Pipeline,
    appsink: AppSink,
    encoder: gst::Element,
    codec: Codec,
    key_interval: u32,
    poking: Arc<AtomicBool>,
    /// The `started` event, sent once the pipeline is playing
    started: Value,
}

fn build(start: &Start, out: &Out) -> Result<Share> {
    let monitor = choose(monitors()?, start);
    let source_size = monitor.as_ref().map_or((start.width, start.height), |m| m.size);
    let (width, height) = fit(source_size, (start.width, start.height));
    let fps = start.fps;
    // A keyframe a minute: viewers joining and packet loss get one on request (PLI), and a
    // scheduled 1080p keyframe (~400 KB measured on real desktop content) is a burst larger than a
    // second's budget, which kept Chromium's bandwidth estimate from ever rising
    let key_interval = fps * 60;

    let pipeline = gst::Pipeline::with_name("share");
    let mut src = gst::ElementFactory::make("d3d11screencapturesrc").property("show-cursor", true);
    src = match &monitor {
        Some(m) => src.property("monitor-handle", m.handle),
        None => src.property("monitor-index", -1i32),
    };
    let src = src.build().context("d3d11screencapturesrc unavailable")?;
    let d3d11 = "video/x-raw(memory:D3D11Memory)";
    let caps = |s: String| -> Result<gst::Element> {
        Ok(gst::ElementFactory::make("capsfilter").property("caps", s.parse::<gst::Caps>()?).build()?)
    };
    let src_caps = caps(d3d11.to_owned())?;
    // Constant output rate: the encoder's rate control budgets against the declared rate
    let rate = make("videorate")?;
    let rate_caps = caps(format!("{d3d11},framerate={fps}/1"))?;
    // One frame, newest wins: a busy encoder drops a frame instead of stalling the source
    let queue = gst::ElementFactory::make("queue")
        .property_from_str("leaky", "downstream")
        .property("max-size-buffers", 1u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .build()?;
    let convert = make("d3d11convert")?;
    let enc_caps = caps(format!("{d3d11},format=NV12,width={width},height={height},framerate={fps}/1"))?;

    let encoder = amf_encoder(start.codec)?;
    set_int(&encoder, "bitrate", start.kbps)?;
    // Latency-constrained VBR capped at the target: AMF's real-time mode, measured with the
    // smallest keyframes (248 KB against 408 KB by default at 1080p) and a gentle first frame
    if encoder.find_property("rate-control").is_some() {
        encoder.set_property_from_str("rate-control", "lcvbr");
    }
    let _ = set_int(&encoder, "max-bitrate", start.kbps);
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

    pipeline.add_many([&src, &src_caps, &rate, &rate_caps, &queue, &convert, &enc_caps, &encoder, &profile, &parser])?;
    pipeline.add(appsink.upcast_ref::<gst::Element>())?;
    gst::Element::link_many([&src, &src_caps, &rate, &rate_caps, &queue, &convert, &enc_caps, &encoder, &profile, &parser])
        .context("capture pipeline link failed")?;
    parser.link(&appsink).context("appsink link failed")?;

    // AMF AV1 ignores force-key-unit: shorten gop-size to 1 and restore it on the next
    // buffer out of the encoder (native client BUG-32; costs one extra keyframe)
    let poking = Arc::new(AtomicBool::new(false));
    if start.codec == Codec::Av1
        && let Some(pad) = encoder.static_pad("src")
    {
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

    let encoder_name = encoder.factory().map(|f| f.name().to_string()).unwrap_or_default();
    let started = json!({
        "type": "started",
        "encoder": encoder_name,
        "monitor": monitor.as_ref().map(|m| &m.label),
        "source": [source_size.0, source_size.1],
        "size": [width, height],
        "fps": fps,
    });
    Ok(Share { pipeline, appsink, encoder, codec: start.codec, key_interval, poking, started })
}

/// Profile and level from the first SPS (NAL type 7) in an Annex B access unit: what the
/// viewers really get, against the `profile-level-id` the SDP promised.
fn h264_sps(au: &[u8]) -> Option<Value> {
    let mut i = 0;
    while i + 3 < au.len() {
        if au[i..i + 3] == [0, 0, 1] {
            let nal = &au[i + 3..];
            if nal.first().map(|b| b & 0x1f) == Some(7) && nal.len() >= 4 {
                let (profile, constraints, level) = (nal[1], nal[2], nal[3]);
                return Some(json!({
                    "type": "stream",
                    "profile_idc": profile,
                    // constraint_set1 on baseline (66) means Constrained Baseline, what 42e01f is
                    "constrained_baseline": profile == 66 && constraints & 0x40 != 0,
                    "profile_level_id": format!("{profile:02x}{constraints:02x}{level:02x}"),
                }));
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    None
}

impl Share {
    fn keyframe(&self) {
        match self.codec {
            Codec::H264 => {
                self.encoder.send_event(
                    gstreamer_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build(),
                );
            }
            Codec::Av1 => {
                // Arm before writing: the probe runs on the streaming thread
                self.poking.store(true, Ordering::Relaxed);
                let _ = set_int(&self.encoder, "gop-size", 1);
            }
        }
    }

    fn bitrate(&self, kbps: u32) -> Result<()> {
        let _ = set_int(&self.encoder, "max-bitrate", kbps.max(100));
        set_int(&self.encoder, "bitrate", kbps.max(100))
    }
}

/// Pulls encoded frames and writes them out until `stop`, reporting stats every 2 s.
fn pump(share: &Share, out: &Out, stop: &AtomicBool) -> Result<()> {
    let bus = share.pipeline.bus().context("pipeline has no bus")?;
    let (mut frames, mut keys, mut bytes, mut since) = (0u64, 0u64, 0u64, Instant::now());
    let mut keys_in_a_row = 0u32;
    let mut reported_sps = share.codec != Codec::H264;
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
        if keys_in_a_row >= 3 && share.codec == Codec::Av1 && !share.poking.load(Ordering::Relaxed) {
            let _ = set_int(&share.encoder, "gop-size", share.key_interval);
            keys_in_a_row = 0;
        }
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
            event(out, json!({
                "type": "stats",
                "fps": (frames as f64 / s * 10.0).round() / 10.0,
                "kbps": (bytes as f64 * 8.0 / 1000.0 / s).round(),
                "keyframes": keys,
            }));
            (frames, keys, bytes, since) = (0, 0, 0, Instant::now());
        }
    }
    Ok(())
}

enum Command {
    Start(Value),
    Keyframe,
    Bitrate(u32),
    Stop,
}

/// Elements the capture graph is built from. AMF is not among them: it registers only on a
/// machine with an AMD GPU, and the packaging self-check runs on CI without one.
const REQUIRED: &[&str] = &[
    "d3d11screencapturesrc", "d3d11convert", "videorate", "capsfilter", "queue", "appsink",
    "h264parse", "av1parse",
];

fn main() -> Result<()> {
    // `--check`: the packaging self-check -- does this bundle load everything by itself?
    if std::env::args().any(|a| a == "--check") {
        gst::init()?;
        let missing: Vec<_> = REQUIRED.iter().filter(|n| gst::ElementFactory::find(n).is_none()).collect();
        let amf = ["amfh264enc", "amfav1enc"].map(|n| gst::ElementFactory::find(n).is_some());
        println!("{}", json!({ "missing": missing, "amf_h264": amf[0], "amf_av1": amf[1] }));
        if !missing.is_empty() {
            bail!("missing elements: {missing:?}");
        }
        return Ok(());
    }
    let out: Out = Arc::new(Mutex::new(std::io::BufWriter::new(std::io::stdout())));
    // Every failure from here on is reported before exiting, so the app falls back to its own
    // capture instead of waiting on a helper that is gone
    let fail = |e: anyhow::Error| {
        event(&out, json!({ "type": "error", "message": format!("{e:#}") }));
        Err(e)
    };
    if let Err(e) = gst::init() {
        return fail(e.into());
    }
    let (tx, rx) = std::sync::mpsc::channel::<Command>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            let cmd = match v.get("cmd").and_then(Value::as_str) {
                Some("start") => Command::Start(v),
                Some("keyframe") => Command::Keyframe,
                Some("bitrate") => Command::Bitrate(v.get("kbps").and_then(Value::as_u64).unwrap_or(0) as u32),
                Some("stop") => Command::Stop,
                _ => continue,
            };
            if tx.send(cmd).is_err() {
                return;
            }
        }
        let _ = tx.send(Command::Stop); // EOF: the app is gone
    });

    let start = loop {
        match rx.recv() {
            Ok(Command::Start(v)) => break v,
            Ok(Command::Stop) | Err(_) => return Ok(()),
            Ok(_) => {}
        }
    };
    let share = match parse_start(&start).and_then(|s| build(&s, &out)) {
        Ok(share) => share,
        Err(e) => return fail(e),
    };
    if let Err(e) = share.pipeline.set_state(gst::State::Playing) {
        let _ = share.pipeline.set_state(gst::State::Null);
        return fail(anyhow::Error::new(e).context("could not start the capture"));
    }
    event(&out, share.started.clone());

    let share = Arc::new(share);
    let stop = Arc::new(AtomicBool::new(false));
    let pumping = {
        let (share, out, stop) = (share.clone(), out.clone(), stop.clone());
        std::thread::spawn(move || {
            if let Err(e) = pump(&share, &out, &stop) {
                event(&out, json!({ "type": "error", "message": format!("{e:#}") }));
            }
            stop.store(true, Ordering::Relaxed);
        })
    };
    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Command::Keyframe) => share.keyframe(),
            Ok(Command::Bitrate(kbps)) => {
                if let Err(e) = share.bitrate(kbps) {
                    event(&out, json!({ "type": "warning", "message": format!("bitrate: {e:#}") }));
                }
            }
            Ok(Command::Stop) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Command::Start(_)) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    stop.store(true, Ordering::Relaxed);
    let _ = pumping.join();
    let _ = share.pipeline.set_state(gst::State::Null);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{fit, h264_sps};

    #[test]
    fn h264_sps_reads_profile_and_level() {
        // AUD, then an SPS for Constrained Baseline 3.1 (42e01f), then an IDR slice
        let au = [0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x1f, 0xda, 0, 0, 1, 0x65, 0x88];
        let sps = h264_sps(&au).expect("an SPS");
        assert_eq!(sps["profile_level_id"], "42e01f");
        assert_eq!(sps["constrained_baseline"], true);
        // High 5.1 (640033) is not Constrained Baseline
        let high = h264_sps(&[0, 0, 1, 0x67, 0x64, 0x00, 0x33, 0xac]).expect("an SPS");
        assert_eq!(high["profile_level_id"], "640033");
        assert_eq!(high["constrained_baseline"], false);
        assert!(h264_sps(&[0, 0, 1, 0x65, 0x88]).is_none());
    }

    #[test]
    fn fit_keeps_the_source_shape_inside_the_box() {
        let box_1080p = (1920, 1080);
        assert_eq!(fit((1080, 1920), box_1080p), (608, 1080));
        assert_eq!(fit((3440, 1440), box_1080p), (1920, 804));
        assert_eq!(fit((3840, 2160), box_1080p), (1920, 1080));
        assert_eq!(fit((1280, 720), box_1080p), (1280, 720));
        assert_eq!(fit((1366, 767), box_1080p), (1366, 766));
    }
}
