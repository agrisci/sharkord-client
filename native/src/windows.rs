//! Windows: DXGI desktop duplication (FFmpeg's `ddagrab`) of the monitor the app picked, converted
//! on the GPU (`scale_d3d11`, BT.709) and encoded by the GPU's own encoder -- AMD AMF, NVIDIA NVENC
//! or Intel Quick Sync, whichever opens on the adapter that drives that monitor (duplication only
//! works there). No copy through memory: capture, conversion and encoder share one D3D11 device.
//!
//! Two threads, as on Linux: the capture thread converts each new desktop picture (DXGI only sends
//! on damage; `ddagrab`'s repeats are skipped), and the encoder thread (`ffmpeg::run`) encodes at the
//! share's rate. The capture is rebuilt when DXGI loses it (a UAC prompt, a mode change, a
//! fullscreen game), while the encoder repeats the last picture.

use std::ffi::c_int;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use ffmpeg_sys_next as ff;
use serde_json::{Value, json};

use crate::ffmpeg::{self, Api, BufRef, Frame, Pacing, Shared, check as ok, cstr};
use crate::{Codec, Out, Start, fit};

/// The encode APIs, in the order they are tried (`SHARKORD_ENCODE_API=amf|nvenc|qsv` forces one).
/// Each only opens on its vendor's GPU.
fn apis() -> Vec<Api> {
    match std::env::var("SHARKORD_ENCODE_API").as_deref() {
        Ok("amf") => vec![Api::Amf],
        Ok("nvenc") => vec![Api::Nvenc],
        Ok("qsv") => vec![Api::Qsv],
        _ => vec![Api::Amf, Api::Nvenc, Api::Qsv],
    }
}

/// One monitor as DXGI enumerates it: the adapter driving it and its output index there (what
/// FFmpeg's D3D11 device and `ddagrab` take).
#[derive(Clone)]
struct Monitor {
    adapter: u32,
    adapter_name: String,
    output: u32,
    label: String,
    primary: bool,
    /// Quarter turns clockwise that make the duplicated desktop upright: DXGI hands a rotated
    /// monitor over in its panel's orientation (a portrait 1080x1920 as 1920x1080)
    rotate: u32,
}

fn wide(s: &[u16]) -> String {
    String::from_utf16_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())])
}

/// Windows' friendly monitor names by GDI device name (`\\.\DISPLAY1`): what Electron's
/// `Display.label` is.
fn friendly_names() -> Vec<(String, String)> {
    use windows::Win32::Devices::Display::*;
    let mut names = Vec::new();
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).is_err() {
            return names;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).is_err() {
            return names;
        }
        for p in &paths[..np as usize] {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            source.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: p.sourceInfo.adapterId,
                id: p.sourceInfo.id,
            };
            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            target.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                adapterId: p.targetInfo.adapterId,
                id: p.targetInfo.id,
            };
            if DisplayConfigGetDeviceInfo(&mut source.header) == 0 && DisplayConfigGetDeviceInfo(&mut target.header) == 0 {
                names.push((wide(&source.viewGdiDeviceName), wide(&target.monitorFriendlyDeviceName)));
            }
        }
    }
    names
}

/// Every monitor on the desktop, by adapter.
fn monitors() -> Result<Vec<Monitor>> {
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270};
    use windows::Win32::Graphics::Dxgi::*;
    let names = friendly_names();
    let mut found = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let adapter_name = adapter.GetDesc1().map(|d| wide(&d.Description)).unwrap_or_default();
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                if let Ok(desc) = output.GetDesc()
                    && desc.AttachedToDesktop.as_bool()
                {
                    let gdi = wide(&desc.DeviceName);
                    let r = desc.DesktopCoordinates;
                    found.push(Monitor {
                        adapter: a,
                        adapter_name: adapter_name.clone(),
                        output: o,
                        label: names.iter().find(|(g, _)| *g == gdi).map_or(gdi.clone(), |(_, n)| n.clone()),
                        // The primary monitor is the one at the desktop's origin
                        primary: r.left == 0 && r.top == 0,
                        rotate: match desc.Rotation {
                            DXGI_MODE_ROTATION_ROTATE90 => 1,
                            DXGI_MODE_ROTATION_ROTATE180 => 2,
                            DXGI_MODE_ROTATION_ROTATE270 => 3,
                            _ => 0,
                        },
                    });
                }
                o += 1;
            }
            a += 1;
        }
    }
    Ok(found)
}

/// The monitor the app picked: by label (and primary, for two monitors of the same model), else
/// the primary one.
fn choose(start_label: Option<&str>, primary: bool) -> Result<Monitor> {
    let mut all = monitors()?;
    let by_label = |m: &Monitor| Some(m.label.as_str()) == start_label;
    let i = all.iter().position(|m| by_label(m) && m.primary == primary)
        .or_else(|| all.iter().position(by_label))
        .or_else(|| all.iter().position(|m| m.primary))
        .or(if all.is_empty() { None } else { Some(0) })
        .ok_or_else(|| anyhow!("no monitor to capture"))?;
    Ok(all.swap_remove(i))
}

/// A D3D11 device on one adapter.
struct Device {
    hw: BufRef,
    api: Api,
}

unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    fn open(adapter: u32, api: Api) -> Result<Device> {
        let mut hw = ptr::null_mut();
        let index = cstr(&adapter.to_string());
        ok(
            unsafe { ff::av_hwdevice_ctx_create(&mut hw, ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA, index.as_ptr(), ptr::null_mut(), 0) },
            "D3D11 device",
        )?;
        Ok(Device { hw: BufRef(hw), api })
    }
}

/// A filter of `graph`, initialized with `args`.
unsafe fn filter(graph: *mut ff::AVFilterGraph, name: &str, args: &str, setup: impl FnOnce(*mut ff::AVFilterContext)) -> Result<*mut ff::AVFilterContext> {
    unsafe {
        let f = ff::avfilter_graph_alloc_filter(graph, ff::avfilter_get_by_name(cstr(name).as_ptr()), cstr(name).as_ptr());
        if f.is_null() {
            bail!("{name} is not in this FFmpeg");
        }
        setup(f);
        ok(ff::avfilter_init_str(f, cstr(args).as_ptr()), name)?;
        Ok(f)
    }
}

/// A filter chain from a buffer source or `ddagrab` to a buffer sink.
struct Graph {
    graph: *mut ff::AVFilterGraph,
    head: *mut ff::AVFilterContext,
    sink: *mut ff::AVFilterContext,
}

unsafe impl Send for Graph {}

impl Graph {
    /// `chain` after `head` (made by `make_head`), each with its arguments and whether it takes
    /// the device.
    fn new(dev: &Device, make_head: impl FnOnce(*mut ff::AVFilterGraph) -> Result<*mut ff::AVFilterContext>, chain: &[(&str, String)]) -> Result<Graph> {
        unsafe {
            let graph = ff::avfilter_graph_alloc();
            if graph.is_null() {
                bail!("filter graph");
            }
            let mut me = Graph { graph, head: ptr::null_mut(), sink: ptr::null_mut() };
            me.head = make_head(graph)?;
            let mut last = me.head;
            for (name, args) in chain {
                let f = filter(graph, name, args, |f| {
                    if *name == "hwupload" {
                        (*f).hw_device_ctx = dev.hw.new_ref();
                    }
                    // QSV maps the converted frames, which needs a fixed pool (our scale_d3d11
                    // patch): 10 pictures, and room for those the encoder holds on to
                    if *name == "scale_d3d11" && dev.api == Api::Qsv {
                        (*f).extra_hw_frames = 8;
                    }
                })?;
                ok(ff::avfilter_link(last, 0, f, 0), name)?;
                last = f;
            }
            me.sink = filter(graph, "buffersink", "", |_| {})?;
            ok(ff::avfilter_link(last, 0, me.sink, 0), "buffersink")?;
            ok(ff::avfilter_graph_config(graph, ptr::null_mut()), "filter graph")?;
            Ok(me)
        }
    }

    /// The next frame out of the sink.
    fn pull(&mut self) -> Result<Option<Frame>> {
        let out = Frame::alloc();
        let r = unsafe { ff::av_buffersink_get_frame(self.sink, out.0) };
        if r == ff::AVERROR(ff::EAGAIN) {
            return Ok(None);
        }
        ok(r, "frame")?;
        Ok(Some(out))
    }

    /// Converts one frame (a buffer source head).
    fn convert(&mut self, frame: &Frame) -> Result<Frame> {
        unsafe { ok(ff::av_buffersrc_add_frame_flags(self.head, frame.0, ff::AV_BUFFERSRC_FLAG_KEEP_REF as c_int), "convert")? };
        self.pull()?.ok_or_else(|| anyhow!("the converter gave no frame"))
    }
}

impl Drop for Graph {
    fn drop(&mut self) {
        unsafe { ff::avfilter_graph_free(&mut self.graph) }
    }
}

/// A buffer source for frames of `format` (a hardware pool, or `sw` in memory) at `size`.
fn source(graph: *mut ff::AVFilterGraph, frames: Option<*mut ff::AVBufferRef>, sw: ff::AVPixelFormat, size: (u32, u32)) -> Result<*mut ff::AVFilterContext> {
    unsafe {
        let src = ff::avfilter_graph_alloc_filter(graph, ff::avfilter_get_by_name(c"buffer".as_ptr()), c"in".as_ptr());
        if src.is_null() {
            bail!("buffer source");
        }
        let par = ff::av_buffersrc_parameters_alloc();
        (*par).format = match frames {
            Some(_) => ff::AVPixelFormat::AV_PIX_FMT_D3D11,
            None => sw,
        } as c_int;
        (*par).width = size.0 as c_int;
        (*par).height = size.1 as c_int;
        (*par).time_base = ff::AVRational { num: 1, den: 1_000_000 };
        (*par).sample_aspect_ratio = ff::AVRational { num: 1, den: 1 };
        (*par).hw_frames_ctx = frames.unwrap_or(ptr::null_mut());
        let r = ff::av_buffersrc_parameters_set(src, par);
        ff::av_free(par as *mut _);
        ok(r, "buffer source parameters")?;
        ok(ff::avfilter_init_str(src, ptr::null()), "buffer source")?;
        Ok(src)
    }
}

/// The conversion to the encoder's input: NV12 at `out`, BT.709 and turned `rotate` quarter turns
/// (our `scale_d3d11` patch), mapped into Quick Sync's frames for QSV.
fn to_encoder(api: Api, out: (u32, u32), rotate: u32) -> Vec<(&'static str, String)> {
    let mut chain = vec![("scale_d3d11", format!("width={}:height={}:format=nv12:rotate={rotate}", out.0, out.1))];
    if api == Api::Qsv {
        chain.push(("hwmap", "derive_device=qsv".to_owned()));
    }
    chain
}

/// A converter from BGRA pictures in memory: the probe's and the tests' input.
fn memory_converter(dev: &Device, size: (u32, u32)) -> Result<Graph> {
    let mut chain = vec![("hwupload", String::new())];
    chain.extend(to_encoder(dev.api, size, 0));
    Graph::new(dev, |g| source(g, None, ff::AVPixelFormat::AV_PIX_FMT_BGRA, size), &chain)
}

/// Whether `dev` encodes `codec` with its API: a small picture uploaded, converted and encoded.
fn can_encode(dev: &Device, codec: Codec) -> bool {
    let size = (1280, 720);
    let attempt = || -> Result<bool> {
        let mut conv = memory_converter(dev, size)?;
        let pic = Frame::alloc();
        unsafe {
            (*pic.0).format = ff::AVPixelFormat::AV_PIX_FMT_BGRA as c_int;
            (*pic.0).width = size.0 as c_int;
            (*pic.0).height = size.1 as c_int;
            ok(ff::av_frame_get_buffer(pic.0, 0), "frame")?;
            ptr::write_bytes((*pic.0).data[0], 64, (*pic.0).linesize[0] as usize * size.1 as usize);
        }
        Ok(ffmpeg::can_encode(dev.api, codec, &conv.convert(&pic)?))
    };
    attempt().unwrap_or(false)
}

/// The first API that encodes on `adapter`, and what it encodes.
fn pick(adapter: u32) -> Option<(Device, bool, bool)> {
    apis().into_iter().find_map(|api| {
        let dev = Device::open(adapter, api).ok()?;
        let (h264, av1) = (can_encode(&dev, Codec::H264), can_encode(&dev, Codec::Av1));
        (h264 || av1).then_some((dev, h264, av1))
    })
}

/// `--check`: which codecs this machine encodes, on the first adapter that encodes at all.
pub fn check() -> Value {
    use windows::Win32::Graphics::Dxgi::*;
    let adapters: Vec<(u32, String)> = unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return json!({ "missing": ["dxgi"], "h264": false, "av1": false });
        };
        (0..).map_while(|i| factory.EnumAdapters1(i).ok().map(|a| (i, a.GetDesc1().map(|d| wide(&d.Description)).unwrap_or_default()))).collect()
    };
    for (i, name) in adapters {
        if let Some((dev, h264, av1)) = pick(i) {
            return json!({ "missing": [], "h264": h264, "av1": av1, "api": dev.api.name(), "device": name });
        }
    }
    json!({ "missing": ["encoder"], "h264": false, "av1": false })
}

/// What the capture needs to stay alive: nothing on Windows (the portal session on Linux).
pub type Capture = ();

pub struct Share {
    device: Arc<Device>,
    monitor: Monitor,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    capture: Mutex<Option<JoinHandle<()>>>,
    codec: Codec,
    fps: u32,
    kbps: AtomicU32,
    keyframe: AtomicBool,
}

/// How long a lost capture is retried before the share gives up.
const REGAIN: Duration = Duration::from_secs(5);

/// The desktop duplication of `m` on `dev`, repeats included (`ddagrab` sends its last picture again
/// when DXGI has no new one: never blocking longer than a frame, so `stop` is seen).
fn grab(dev: &Device, m: &Monitor, fps: u32) -> Result<Graph> {
    // 8-bit BGRA even from an HDR desktop (DXGI converts): H.264/AV1 here are 8-bit SDR
    let args = format!("output_idx={}:framerate={fps}:draw_mouse=1:output_fmt=bgra:dup_frames=1", m.output);
    Graph::new(dev, |g| unsafe { filter(g, "ddagrab", &args, |f| (*f).hw_device_ctx = dev.hw.new_ref()) }, &[])
}

/// The capture thread: converts each new desktop picture for the encoder until `stop`.
fn capture(dev: &Device, m: &Monitor, fps: u32, max: (u32, u32), shared: &Shared, stop: &AtomicBool) -> Result<()> {
    let mut grabbed: Option<Graph> = None;
    let mut conv: Option<(Graph, (u32, u32))> = None;
    let mut prev: Option<Frame> = None;
    let mut lost: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        let g = match grabbed.as_mut() {
            Some(g) => g,
            None => match grab(dev, m, fps) {
                Ok(g) => {
                    lost = None;
                    grabbed.insert(g)
                }
                // Lost (a UAC prompt, a mode change, a fullscreen game): retried a while
                Err(e) if lost.is_some_and(|t| t.elapsed() < REGAIN) => {
                    let _ = e;
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                Err(e) => return Err(e.context("desktop duplication")),
            },
        };
        let frame = match g.pull() {
            Ok(Some(f)) => f,
            Ok(None) => continue,
            Err(e) => {
                eprintln!("capture: {e:#}, starting again");
                grabbed = None;
                prev = None;
                lost.get_or_insert_with(Instant::now);
                continue;
            }
        };
        // `ddagrab`'s repeat of its last picture: the same buffer (held in `prev`, so not reused)
        let buffer = |f: &Frame| unsafe { (*(*f.0).buf[0]).buffer };
        if prev.as_ref().is_some_and(|p| buffer(p) == buffer(&frame)) {
            continue;
        }
        let size = frame.size();
        // The desktop as it is seen: a portrait monitor's picture turned upright
        let upright = if m.rotate % 2 == 1 { (size.1, size.0) } else { size };
        let converted = (|| -> Result<Frame> {
            if conv.as_ref().is_none_or(|c| c.1 != size) {
                conv = None;
                let frames = unsafe { (*frame.0).hw_frames_ctx };
                let chain = to_encoder(dev.api, fit(upright, max), m.rotate);
                conv = Some((Graph::new(dev, |g| source(g, Some(frames), ff::AVPixelFormat::AV_PIX_FMT_BGRA, size), &chain)?, size));
            }
            conv.as_mut().expect("converter").0.convert(&frame)
        })()?;
        prev = Some(frame);
        shared.put(converted, upright, Some(json!({
            "type": "input", "format": "BGRA", "size": [size.0, size.1], "memory": "d3d11",
            "adapter": m.adapter, "output": m.output, "rotate": m.rotate * 90,
        })));
    }
    Ok(())
}

impl Share {
    pub fn start(start: &Start, _out: &Out, _capture: Capture) -> Result<Share> {
        let monitor = choose(start.label.as_deref(), start.primary)?;
        let (device, ..) = pick(monitor.adapter)
            .ok_or_else(|| anyhow!("{} can't encode H.264 or AV1 (AMF, NVENC or Quick Sync)", monitor.adapter_name))?;
        let device = Arc::new(device);
        let shared = Arc::new(Shared::new());
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (device, shared, stop) = (device.clone(), shared.clone(), stop.clone());
            let m = monitor.clone();
            let (fps, max) = (start.fps, (start.width, start.height));
            std::thread::Builder::new().name("capture".into()).spawn(move || {
                if let Err(e) = capture(&device, &m, fps, max, &shared, &stop) {
                    shared.fail(&e);
                }
            })?
        };
        Ok(Share {
            device,
            monitor,
            shared,
            stop,
            capture: Mutex::new(Some(thread)),
            codec: start.codec,
            fps: start.fps,
            kbps: AtomicU32::new(start.kbps),
            keyframe: AtomicBool::new(false),
        })
    }

    /// Encodes at the share's rate until `stop` (or the capture fails).
    pub fn run(&self, out: &Out, stop: &AtomicBool) -> Result<()> {
        let pacing = Pacing {
            api: self.device.api,
            codec: self.codec,
            fps: self.fps,
            kbps: &self.kbps,
            keyframe: &self.keyframe,
            started: json!({ "monitor": self.monitor.label, "device": self.monitor.adapter_name }),
        };
        ffmpeg::run(&pacing, &self.shared, out, stop)
    }

    pub fn finish(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.capture.lock().expect("capture").take() {
            let _ = t.join();
        }
    }

    pub fn keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    pub fn bitrate(&self, kbps: u32) -> Result<()> {
        self.kbps.store(kbps.max(100), Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The primary monitor's picture in memory (BGRA), and the same scrolled by 120 rows: a real
    /// desktop, where a scroll is the typical burst (random patterns don't compress under any cap).
    fn desktop(dev: &Device) -> [Frame; 2] {
        let mut g = grab(dev, &choose(None, true).unwrap(), 60).unwrap();
        let hw = loop {
            if let Some(f) = g.pull().unwrap() {
                break f;
            }
        };
        [0usize, 120].map(|shift| unsafe {
            let sw = Frame::alloc();
            assert!(ff::av_hwframe_transfer_data(sw.0, hw.0, 0) >= 0);
            let (w, h) = hw.size();
            let pic = Frame::alloc();
            (*pic.0).format = ff::AVPixelFormat::AV_PIX_FMT_BGRA as c_int;
            (*pic.0).width = w as c_int;
            (*pic.0).height = h as c_int;
            assert!(ff::av_frame_get_buffer(pic.0, 0) >= 0);
            let (src, dst) = ((*sw.0).linesize[0] as usize, (*pic.0).linesize[0] as usize);
            for y in 0..h as usize {
                let sy = (y + shift) % h as usize;
                ptr::copy_nonoverlapping((*sw.0).data[0].add(sy * src), (*pic.0).data[0].add(y * dst), w as usize * 4);
            }
            pic
        })
    }

    /// Needs a GPU that encodes H.264: `cargo test -- --ignored`. The bench of `crate::ffmpeg` on
    /// this desktop (scrolled every other frame), through the upload path, on each API that opens.
    #[test]
    #[ignore]
    fn encodes_the_desktop_with_keyframes_only_on_request() {
        let mut ran = false;
        for api in apis() {
            let Some(dev) = (0..4).find_map(|a| Device::open(a, api).ok().filter(|d| can_encode(d, Codec::H264))) else { continue };
            let pictures = desktop(&dev);
            let mut conv = memory_converter(&dev, pictures[0].size()).unwrap();
            ffmpeg::bench::keyframes_on_request(api, |i| conv.convert(&pictures[i % 2]).unwrap());
            ran = true;
        }
        assert!(ran, "no encoder opened");
    }
}
