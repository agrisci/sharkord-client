//! Linux: the portal's PipeWire stream (`capture`), converted and encoded on the GPU with FFmpeg
//! (`encode`: Vulkan video, else VA-API), paced here.
//!
//! Two threads: the capture thread converts the newest frame the compositor sent into the
//! encoder's NV12 at the share's size (the compositor's buffer goes back as soon as the GPU has
//! read it), and the encoder thread (`ffmpeg::run`) encodes at the share's rate, repeating the last
//! picture while the screen is still (the compositor only sends on damage).

mod capture;
mod encode;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::ffmpeg::{self, Pacing, Shared};
use crate::{Codec, Out, Start, fit};
use encode::{Api, Convert, Device, Frame, Input};

/// What to capture: the portal's stream (the session must stay alive as long as the capture), or
/// for development a PipeWire node of the user's own session (`SHARKORD_TEST_NODE`, no dialog).
pub struct Capture {
    pub portal: Option<crate::portal::Portal>,
    pub node: u32,
}

/// Encodes one small frame with `codec`: the probe's proof that the whole path works here.
fn can_encode(dev: &Device, codec: Codec) -> bool {
    // Not too small: RADV's AV1 encoder rejects 256x144
    let size = (1280, 720);
    let pixels = vec![64u8; 1280 * 720 * 4];
    let frame = Convert::new(dev, Input::Memory, ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_BGR0, size, size)
        .and_then(|mut conv| conv.convert(&encode::memory_frame(size, false, &pixels, 1280 * 4)?));
    frame.is_ok_and(|f| ffmpeg::can_encode(dev.api, codec, &f))
}

/// The first render node and API (Vulkan first; `SHARKORD_ENCODE_API=vaapi|vulkan` forces one)
/// that encodes, and what it encodes.
fn pick() -> Option<(Device, bool, bool)> {
    let apis = match std::env::var("SHARKORD_ENCODE_API").as_deref() {
        Ok("vaapi") => vec![Api::Vaapi],
        Ok("vulkan") => vec![Api::Vulkan],
        _ => vec![Api::Vulkan, Api::Vaapi],
    };
    pick_from(&apis)
}

fn pick_from(apis: &[Api]) -> Option<(Device, bool, bool)> {
    for &api in apis {
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

/// Where the system's Vulkan driver can't encode H.264 (Fedora and openSUSE build Mesa without it,
/// NVIDIA aside) and the bundled ones are here, runs this same helper again on those: only this
/// process loads them, never the app. Called first thing, before the portal's dialog. Ahead of the
/// system's VA-API, which may encode there too (with RPM Fusion's driver): VBR under the frame cap
/// beats VA-API's CBR, which pads a still screen. On the bundled drivers the helper still falls back
/// to VA-API where they can't encode (AMD before VCN, Intel before Gen12).
pub fn prefer_bundled_driver() {
    use std::os::unix::process::CommandExt;
    if std::env::var_os(BUNDLED).is_some()
        || std::env::var("SHARKORD_ENCODE_API").as_deref() == Ok("vaapi")
        || pick_from(&[Api::Vulkan]).is_some_and(|(_, h264, _)| h264)
    {
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
        let shared = Arc::new(Shared::new());
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
                        Ok(frame) => shared.put(frame, format.size, Some(json!({
                            "type": "input", "format": if format.alpha { "BGRA" } else { "BGRx" },
                            "size": [format.size.0, format.size.1],
                            "memory": if format.modifier.is_some() { "dmabuf" } else { "shm" },
                            "modifier": format.modifier.map(|m| format!("{m:#x}")),
                        }))),
                        Err(e) => {
                            shared.fail(&e);
                            stop.store(true, Ordering::Relaxed);
                        }
                    }
                });
                if let Err(e) = result {
                    failed.fail(&e);
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
        let pacing = Pacing {
            api: self.device.api,
            codec: self.codec,
            fps: self.fps,
            kbps: &self.kbps,
            keyframe: &self.keyframe,
            started: json!({ "monitor": null, "device": self.device.render_node }),
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
