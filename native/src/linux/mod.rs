//! Linux: the portal's PipeWire stream (`capture`), converted and encoded on the GPU with FFmpeg
//! (`encode`: Vulkan video, else VA-API), paced here.
//!
//! Two threads: the capture thread converts the newest frame the compositor sent into the
//! encoder's NV12 at the share's size (the compositor's buffer goes back as soon as the GPU has
//! read it), and the encoder thread encodes at the share's rate, repeating the last picture while
//! the screen is still (the compositor only sends on damage). This replaces GStreamer's
//! `videorate` + leaky queue + `keepalive-time`: frames are stamped by the encoder's own tick, so
//! the compositor's and our clocks never meet, and a busy encoder skips a tick instead of queueing.

mod capture;
mod encode;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::{Codec, Out, Start, event, fit, h264_sps, write_record};
use encode::{Api, Convert, Device, Encoder, Frame, Input, Settings};

/// What to capture: the portal's stream (the session must stay alive as long as the capture), or
/// for development a PipeWire node of the user's own session (`SHARKORD_TEST_NODE`, no dialog).
pub struct Capture {
    pub portal: Option<crate::portal::Portal>,
    pub node: u32,
}

/// Encodes one small frame with `codec`: the probe's proof that the whole path works here.
fn can_encode(dev: &Device, codec: Codec) -> bool {
    let attempt = || -> Result<()> {
        // Not too small: RADV's AV1 encoder rejects 256x144
        let size = (1280, 720);
        let mut conv = Convert::new(dev, Input::Memory, ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_BGR0, size, size)?;
        let pixels = vec![64u8; 1280 * 720 * 4];
        let frame = conv.convert(&encode::memory_frame(size, false, &pixels, 1280 * 4)?)?;
        let frames = unsafe { (*frame.0).hw_frames_ctx };
        let mut enc = Encoder::open(dev, frames, &Settings { codec, size, fps: 60, kbps: 2000 })?;
        let mut got = false;
        for i in 0..8 {
            enc.send(&frame.share(), i, i == 0)?;
            enc.receive(|_, key, _| {
                got |= key;
                Ok(())
            })?;
            if got {
                return Ok(());
            }
        }
        bail!("no keyframe")
    };
    attempt().is_ok()
}

/// The first render node and API (Vulkan first; `SHARKORD_ENCODE_API=vaapi|vulkan` forces one)
/// that encodes, and what it encodes.
fn pick() -> Option<(Device, bool, bool)> {
    let apis = match std::env::var("SHARKORD_ENCODE_API").as_deref() {
        Ok("vaapi") => vec![Api::Vaapi],
        Ok("vulkan") => vec![Api::Vulkan],
        _ => vec![Api::Vulkan, Api::Vaapi],
    };
    for api in apis {
        for node in Device::render_nodes() {
            let Ok(dev) = Device::open(&node, api) else { continue };
            let (h264, av1) = (can_encode(&dev, Codec::H264), can_encode(&dev, Codec::Av1));
            if h264 || av1 {
                return Some((dev, h264, av1));
            }
        }
    }
    None
}

pub fn check() -> Value {
    let driver = if std::env::var_os(BUNDLED).is_some() { "bundled" } else { "system" };
    match pick() {
        Some((dev, h264, av1)) => {
            json!({ "missing": [], "h264": h264, "av1": av1, "api": dev.api.name(), "device": dev.render_node, "driver": driver })
        }
        None => json!({ "missing": ["encoder"], "h264": false, "av1": false, "driver": driver }),
    }
}

/// Set when the helper runs on the bundled Mesa Vulkan drivers
const BUNDLED: &str = "SHARKORD_BUNDLED_DRIVER";

/// The bundled Mesa Vulkan drivers' loader manifests: `resources/native/mesa` next to the packaged
/// helper (`bin/`), or `build/deps/mesa` (scripts/deps/mesa.sh) for the development build.
fn bundled_drivers() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [dir.join("../mesa"), dir.join("../../../build/deps/mesa")].into_iter().find_map(|d| {
        let icds: Vec<String> = ["radeon_icd.json", "intel_icd.json"]
            .iter()
            .map(|f| d.join(f))
            .filter(|p| p.exists())
            .filter_map(|p| p.canonicalize().ok()?.to_str().map(str::to_owned))
            .collect();
        (!icds.is_empty()).then(|| icds.join(":"))
    })
}

/// Where the system's drivers can't encode H.264 (Fedora and openSUSE build Mesa without it) and
/// the bundled ones are here, runs this same helper again on those: only this process loads them,
/// never the app. Called first thing, before the portal's dialog.
pub fn prefer_bundled_driver() {
    use std::os::unix::process::CommandExt;
    if std::env::var_os(BUNDLED).is_some() || pick().is_some_and(|(_, h264, _)| h264) {
        return;
    }
    let Some(icds) = bundled_drivers() else { return };
    let Ok(exe) = std::env::current_exe() else { return };
    let err = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env(BUNDLED, "1")
        .env("VK_DRIVER_FILES", &icds)
        .env("VK_ICD_FILENAMES", &icds)
        .env("VK_LOADER_LAYERS_DISABLE", "~implicit~")
        .exec();
    eprintln!("bundled Vulkan drivers: {err}");
}

/// The newest converted picture, from the capture thread to the encoder thread.
struct Latest {
    frame: Option<Frame>,
    fresh: bool,
    source: (u32, u32),
    format: Option<capture::Format>,
}

struct Shared {
    latest: Mutex<Latest>,
    failed: Mutex<Option<String>>,
    captured: AtomicU64,
}

pub struct Share {
    device: Arc<Device>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    capture: Mutex<Option<JoinHandle<()>>>,
    codec: Codec,
    fps: u32,
    kbps: AtomicU32,
    keyframe: AtomicBool,
    _portal: Capture,
}

// The FFmpeg device and frames are reference-counted and thread-safe to hand between threads
unsafe impl Sync for Device {}
unsafe impl Sync for Share {}

impl Share {
    pub fn start(start: &Start, out: &Out, portal: Capture) -> Result<Share> {
        let (device, ..) = pick().ok_or_else(|| anyhow!("no GPU here encodes H.264 or AV1 (Vulkan video or VA-API)"))?;
        let device = Arc::new(device);
        let modifiers = device.modifiers();
        let shared = Arc::new(Shared {
            latest: Mutex::new(Latest { frame: None, fresh: false, source: (0, 0), format: None }),
            failed: Mutex::new(None),
            captured: AtomicU64::new(0),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let fd = portal.portal.as_ref().map(|p| p.fd.try_clone()).transpose()?;
        let capture = {
            let (device, shared, stop, node, fps, max) = (device.clone(), shared.clone(), stop.clone(), portal.node, start.fps, (start.width, start.height));
            std::thread::Builder::new().name("capture".into()).spawn(move || {
                let mut conv: Option<(Convert, capture::Format)> = None;
                let failed = shared.clone();
                let result = capture::run(fd, node, fps, modifiers, stop.clone(), move |captured| {
                    let format = captured.format;
                    let converted = (|| -> Result<Frame> {
                        if conv.as_ref().is_none_or(|c| c.1 != format) {
                            let input = if format.modifier.is_some() { Input::Drm } else { Input::Memory };
                            let pix = if format.alpha { ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_BGRA } else { ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_BGR0 };
                            conv = None;
                            conv = Some((Convert::new(&device, input, pix, format.size, fit(format.size, max))?, format));
                        }
                        let (c, _) = conv.as_mut().expect("converter");
                        match captured.data {
                            capture::Data::Dmabuf { fd, size, offset, stride } => {
                                // Converted on the GPU straight from the compositor's buffer, which
                                // goes back as soon as that is done (`Device::wait`)
                                let frames = c.drm_frames.as_ref().expect("DRM frame pool");
                                let release = captured.into_release();
                                let frame = encode::drm_frame(frames, format.size, format.alpha, fd, size, offset, stride, format.modifier.unwrap_or(0), ())?;
                                let out = c.convert(&frame)?;
                                device.wait(&out)?;
                                drop(release);
                                Ok(out)
                            }
                            capture::Data::Memory { ptr, len, stride } => {
                                let data = unsafe { std::slice::from_raw_parts(ptr, len) };
                                let frame = encode::memory_frame(format.size, format.alpha, data, stride as usize)?;
                                drop(captured);
                                c.convert(&frame)
                            }
                        }
                    })();
                    match converted {
                        Ok(frame) => {
                            let mut latest = shared.latest.lock().expect("latest");
                            *latest = Latest { frame: Some(frame), fresh: true, source: format.size, format: Some(format) };
                            shared.captured.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            *shared.failed.lock().expect("failed") = Some(format!("{e:#}"));
                            stop.store(true, Ordering::Relaxed);
                        }
                    }
                });
                if let Err(e) = result {
                    failed.failed.lock().expect("failed").get_or_insert(format!("{e:#}"));
                }
            })?
        };
        let _ = out;
        Ok(Share {
            device,
            shared,
            stop,
            capture: Mutex::new(Some(capture)),
            codec: start.codec,
            fps: start.fps,
            kbps: AtomicU32::new(start.kbps),
            keyframe: AtomicBool::new(false),
            _portal: portal,
        })
    }

    /// Encodes at the share's rate until `stop` (or the capture fails).
    pub fn run(&self, out: &Out, stop: &AtomicBool) -> Result<()> {
        let fps = self.fps.max(1);
        let period = Duration::from_nanos(1_000_000_000 / u64::from(fps));
        let mut next = Instant::now();
        let mut enc: Option<(Encoder, (u32, u32))> = None;
        let mut last: Option<Frame> = None;
        let mut kbps = self.kbps.load(Ordering::Relaxed);
        let (mut tick, mut duplicated, mut dropped, mut sent) = (0i64, 0u64, 0u64, 0u64);
        let (mut frames, mut keys, mut bytes, mut since) = (0u64, 0u64, 0u64, Instant::now());
        let mut reported_sps = self.codec != Codec::H264;
        let mut started = false;
        while !stop.load(Ordering::Relaxed) && !self.stop.load(Ordering::Relaxed) {
            if let Some(e) = self.shared.failed.lock().expect("failed").take() {
                bail!(e);
            }
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else if now - next > period {
                // The encoder fell behind: skip the ticks instead of catching up in a burst
                let behind = ((now - next).as_nanos() / period.as_nanos()) as u64;
                dropped += behind;
                tick += behind as i64;
                next = now;
            }
            next += period;

            let (frame, source, format) = {
                let mut latest = self.shared.latest.lock().expect("latest");
                if latest.fresh {
                    latest.fresh = false;
                    last = latest.frame.as_ref().map(Frame::share);
                } else if last.is_some() {
                    duplicated += 1;
                }
                (last.as_ref().map(Frame::share), latest.source, latest.format)
            };
            let Some(frame) = frame else { continue };
            let size = unsafe { ((*frame.0).width as u32, (*frame.0).height as u32) };

            // A new size (the first frame, or the source changed shape): a new encoder, keyed
            let mut key = self.keyframe.swap(false, Ordering::Relaxed);
            if enc.as_ref().is_none_or(|e| e.1 != size) {
                drop(enc.take());
                let frames = unsafe { (*frame.0).hw_frames_ctx };
                let e = Encoder::open(&self.device, frames, &Settings { codec: self.codec, size, fps, kbps })?;
                enc = Some((e, size));
                key = true;
            }
            let (e, _) = enc.as_mut().expect("encoder");
            let wanted = self.kbps.load(Ordering::Relaxed);
            if wanted != kbps {
                kbps = wanted;
                e.set_rate(kbps);
            }
            let name = e.name.clone();
            e.send(&frame, tick, key)?;
            tick += 1;
            sent += 1;
            e.receive(|data, key, pts| {
                if !started {
                    started = true;
                    event(out, json!({
                        "type": "started", "encoder": name, "monitor": null,
                        "source": [source.0, source.1], "size": [size.0, size.1], "fps": fps,
                        "api": self.device.api.name(), "device": self.device.render_node,
                    }));
                    if let Some(f) = format {
                        event(out, json!({
                            "type": "input", "format": if f.alpha { "BGRA" } else { "BGRx" },
                            "size": [f.size.0, f.size.1],
                            "memory": if f.modifier.is_some() { "dmabuf" } else { "shm" },
                            "modifier": f.modifier.map(|m| format!("{m:#x}")),
                        }));
                    }
                }
                if key && !reported_sps && let Some(sps) = h264_sps(data) {
                    event(out, sps);
                    reported_sps = true;
                }
                let pts_us = (pts.max(0) as u64) * 1_000_000 / u64::from(fps);
                write_record(out, 1, u8::from(key), pts_us, data).map_err(|e| anyhow!("stdout closed: {e}"))?;
                frames += 1;
                keys += u64::from(key);
                bytes += data.len() as u64;
                Ok(())
            })?;
            let elapsed = since.elapsed();
            if elapsed >= Duration::from_secs(2) {
                let s = elapsed.as_secs_f64();
                // Totals since the start: pictures captured, encoded, repeated, skipped
                let rate = [self.shared.captured.load(Ordering::Relaxed), sent, duplicated, dropped];
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
