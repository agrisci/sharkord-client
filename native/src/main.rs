//! Native screen capture + hardware encode for the Electron client's screen share.
//!
//! Sharkord's page keeps its own WebRTC connection; the preload swaps each outgoing
//! placeholder frame's payload for a frame from here (see `electron/preload.js`). This
//! process only captures and encodes -- no network.
//!
//! Protocol, one share per process:
//! - stdin, JSON lines: `{"cmd":"start","codec":"h264"|"av1","width","height","fps","kbps",
//!   "label","primary"}` (label/primary pick the monitor on Windows), `{"cmd":"keyframe"}`,
//!   `{"cmd":"bitrate","kbps"}`, `{"cmd":"stop"}`.
//!   EOF means the app is gone: stop and exit.
//! - stdout, records: a 16-byte header (u8 kind, u8 flags, u16 reserved, u32 LE length,
//!   u64 LE pts in microseconds) and the payload. Kind 1 is an encoded frame (flag 1 =
//!   keyframe), kind 2 a JSON event: on Linux `selected` / `cancelled` for the portal dialog,
//!   then `started` once capturing, `input` with the caps reaching the converter (whether frames
//!   stay in GPU memory), `stream` with the first H.264 keyframe's SPS profile and level, `stats`
//!   every 2 s (with videorate's in/out/duplicate/drop totals), `warning`, and `error` before the
//!   process gives up -- every failure after `start` sends one.
//!
//! Windows: each part of the graph and its settings was measured on an RX 9060 XT: DXGI capture
//! straight into `d3d11convert` and AMF with no copies, `videorate` holding the declared rate
//! the encoder budgets against, a one-frame leaky queue so a busy encoder never holds the
//! source back, a keyframe a minute (more on request), no B-frames, and AMF AV1's keyframe
//! poke (it ignores force-key-unit).
//!
//! Linux (Wayland): the same graph from a PipeWire stream the helper gets from the portal
//! (`portal`), copied into VA memory and converted by `vapostproc` for a VA-API encoder. The
//! helper shows the portal's dialog as soon as it starts, before `start`, and reports `selected`
//! (with the source's size) or `cancelled`; Chromium doesn't capture at all, the page builds the
//! share's stream (and its preview) from these frames.

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;
use serde_json::{Value, json};

#[cfg(target_os = "linux")]
mod portal;

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
    // Which monitor (Windows); on Linux the portal's dialog picks it
    #[cfg_attr(not(windows), allow(dead_code))]
    label: Option<String>,
    #[cfg_attr(not(windows), allow(dead_code))]
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

/// The largest even size inside `max` with the source's aspect ratio, never larger than
/// the source -- what a browser does with getDisplayMedia's width/height.
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
/// (AMF's `gop-size` is signed on H.264 and unsigned on AV1), clamped to its range (VA's
/// `key-int-max` stops at 1024). Skips a missing or read-only property rather than panicking.
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

/// The hardware encoder for `codec`: the first factory that builds. AMF (Windows) and VA (Linux)
/// register one per GPU (`amfh264device1enc`, `varenderD129h264enc`); VA's Intel low-power
/// variants (`vah264lpenc`) come after the normal ones.
fn encoder(codec: Codec) -> Result<gst::Element> {
    let c = match codec {
        Codec::H264 => "h264",
        Codec::Av1 => "av1",
    };
    #[cfg(windows)]
    let (names, needs): (Vec<String>, _) = (
        std::iter::once(format!("amf{c}enc")).chain((1..4).map(|i| format!("amf{c}device{i}enc"))).collect(),
        "an AMD GPU",
    );
    #[cfg(not(windows))]
    let (names, needs): (Vec<String>, _) = (
        [format!("va{c}enc"), format!("va{c}lpenc")]
            .into_iter()
            .chain((129..133).flat_map(|d| [format!("varenderD{d}{c}enc"), format!("varenderD{d}{c}lpenc")]))
            .collect(),
        "a GPU with VA-API encoding",
    );
    names
        .iter()
        .find_map(|n| gst::ElementFactory::make(n).build().ok())
        .ok_or_else(|| anyhow!("no {c} encoder (this helper needs {needs})"))
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
    /// The source, and on Linux the copy out of the compositor's buffers
    head: Vec<gst::Element>,
    /// Caps prefix from the head up to `convert`
    memory: &'static str,
    convert: &'static str,
    /// Caps prefix the encoder takes
    encoder_memory: &'static str,
    size: (u32, u32),
    label: Option<String>,
}

/// What the capture needs to stay alive: the portal session on Linux, nothing on Windows.
#[cfg(target_os = "linux")]
type Capture = portal::Portal;
#[cfg(windows)]
type Capture = ();

#[cfg(windows)]
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

/// Linux. Measured on KWin (Renoir and RX 9060 XT):
/// - The frame is copied into VA memory straight away (`vapostproc`, never passthrough): the
///   compositor lends only a few buffers and stops sending while we hold them, which `videorate`
///   and the queue would. It also stops two VA elements importing one DMA-BUF, which races.
/// - No clock, and stamps taken on arrival: a compositor frame carries the compositor's time and
///   a keepalive copy the pipeline's, they drift apart, and with a clock the source sleeps on the
///   gap or `videorate` drops frames as "before the previous buffer".
/// - `keepalive-time` at the frame period: PipeWire only sends when the screen changes, and
///   larger values measurably gated real frames too.
#[cfg(target_os = "linux")]
fn source(start: &Start, capture: &Capture) -> Result<Source> {
    use std::os::fd::AsRawFd;
    let src = gst::ElementFactory::make("pipewiresrc")
        .property("fd", capture.fd.as_raw_fd())
        .property("path", capture.node.to_string())
        .property("keepalive-time", (1000 / start.fps.max(1)).max(1) as i32)
        .property("provide-clock", false)
        .build()
        .context("pipewiresrc unavailable (install GStreamer's PipeWire plugin)")?;
    let epoch = Instant::now();
    src.static_pad("src").context("pipewiresrc has no source pad")?.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(buffer) = info.buffer_mut() {
            let now = gst::ClockTime::from_nseconds(epoch.elapsed().as_nanos() as u64);
            let buffer = buffer.make_mut();
            buffer.set_pts(now);
            buffer.set_dts(now);
        }
        gst::PadProbeReturn::Ok
    });
    let import = gst::ElementFactory::make("vapostproc").property("disable-passthrough", true).build()?;
    Ok(Source {
        head: vec![src, import],
        memory: "video/x-raw(memory:VAMemory)",
        convert: "vapostproc",
        encoder_memory: "video/x-raw(memory:VAMemory)",
        size: capture.size.unwrap_or((start.width, start.height)),
        label: None,
    })
}

struct Share {
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
    /// The `started` event, sent once the pipeline is playing
    started: Value,
    _capture: Capture,
}

fn build(start: &Start, out: &Out, capture: Capture) -> Result<Share> {
    let source = source(start, &capture)?;
    let (width, height) = fit(source.size, (start.width, start.height));
    let fps = start.fps;
    // A keyframe a minute: viewers joining and packet loss get one on request (PLI), and a
    // scheduled 1080p keyframe (~400 KB measured on real desktop content) is a burst larger than a
    // second's budget, which kept Chromium's bandwidth estimate from ever rising. VA's encoders
    // allow at most 1024 frames (`set_int` clamps to it): ~17 s at 60 fps
    let key_interval = fps * 60;

    let pipeline = gst::Pipeline::with_name("share");
    // Linux: nothing needs a clock (PipeWire and keepalive pace the source, the appsink doesn't
    // sync), and pipewiresrc waiting on one stalls it (see `source`)
    #[cfg(target_os = "linux")]
    pipeline.use_clock(None::<&gst::Clock>);
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
    let amf = encoder_name.starts_with("amf");
    set_int(&encoder, "bitrate", start.kbps)?;
    // AMF: latency-constrained VBR capped at the target, its real-time mode, measured with the
    // smallest keyframes (248 KB against 408 KB by default at 1080p) and a gentle first frame.
    // VA: VBR peaking at the target; its default CBR pads a still screen with filler up to the
    // target (27 Mbps measured for a static desktop)
    if encoder.find_property("rate-control").is_some() {
        encoder.set_property_from_str("rate-control", if amf { "lcvbr" } else { "vbr" });
    }
    if !amf {
        let _ = set_int(&encoder, "target-percentage", 100);
    }
    let _ = set_int(&encoder, "max-bitrate", start.kbps);
    let gop = if amf { "gop-size" } else { "key-int-max" };
    if let Err(e) = set_int(&encoder, gop, key_interval) {
        event(out, json!({ "type": "warning", "message": format!("keyframe interval not set: {e}") }));
    }
    if start.codec == Codec::H264 {
        let _ = set_int(&encoder, "b-frames", 0);
    }
    // VA AV1 reorders a ~32-frame group by default: half a second of latency, and holding that
    // many frames drains PipeWire's pool and ends the capture. Level 1 turns it off, and
    // target-usage 1 is the fast end on AMD (70 fps at 4K against 63 by default)
    if !amf && start.codec == Codec::Av1 {
        let _ = set_int(&encoder, "hierarchical-level", 1);
        let _ = set_int(&encoder, "target-usage", 1);
    }
    // The profile the SDP promises: Sharkord negotiates Constrained Baseline (42e01f) for
    // H.264, and AMF and VA take their profile from downstream caps (AMF has no property for it)
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
    // buffer out of the encoder (costs one extra keyframe). VA honours force-key-unit.
    let poke = amf && start.codec == Codec::Av1;
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
        if self.poke {
            // Arm before writing: the probe runs on the streaming thread
            self.poking.store(true, Ordering::Relaxed);
            let _ = set_int(&self.encoder, "gop-size", 1);
        } else {
            self.encoder.send_event(gstreamer_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build());
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
        if frames_total == 0
            && let Some(caps) = share.convert.static_pad("sink").and_then(|p| p.current_caps())
        {
            event(out, json!({ "type": "input", "caps": caps.to_string() }));
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

enum Command {
    Start(Value),
    Keyframe,
    Bitrate(u32),
    Stop,
}

/// Elements the capture graph is built from. The encoders are not among them: they register
/// only with a supported GPU, and the packaging self-check runs on CI without one.
#[cfg(windows)]
const REQUIRED: &[&str] = &[
    "d3d11screencapturesrc", "d3d11convert", "videorate", "capsfilter", "queue", "appsink",
    "h264parse", "av1parse",
];
#[cfg(target_os = "linux")]
const REQUIRED: &[&str] = &[
    "pipewiresrc", "vapostproc", "videorate", "capsfilter", "queue", "appsink", "h264parse", "av1parse",
];

fn main() -> Result<()> {
    // `--check`: does this bundle (Windows) or the system's GStreamer (Linux) load everything, and
    // which codecs can this machine encode? The packaging self-check and, on Linux, the app's
    // startup probe
    if std::env::args().any(|a| a == "--check") {
        gst::init()?;
        let missing: Vec<_> = REQUIRED.iter().filter(|n| gst::ElementFactory::find(n).is_none()).collect();
        let (h264, av1) = (can_encode(Codec::H264), can_encode(Codec::Av1));
        println!("{}", json!({ "missing": missing, "h264": h264, "av1": av1 }));
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

    // Linux: the screen is picked in the portal's dialog now, while the app waits to build the
    // share's stream; `start` only comes once Sharkord has negotiated the codec
    #[cfg(target_os = "linux")]
    let capture = match portal::select() {
        Ok(Some(p)) => {
            event(&out, json!({ "type": "selected", "source": p.size.map(|(w, h)| [w, h]) }));
            p
        }
        Ok(None) => {
            event(&out, json!({ "type": "cancelled" }));
            return Ok(());
        }
        Err(e) => return fail(e),
    };
    #[cfg(windows)]
    let capture = ();

    let start = loop {
        match rx.recv() {
            Ok(Command::Start(v)) => break v,
            Ok(Command::Stop) | Err(_) => return Ok(()),
            Ok(_) => {}
        }
    };
    let share = match parse_start(&start).and_then(|s| build(&s, &out, capture)) {
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
