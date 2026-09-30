//! FFmpeg, as both platforms use it: the hardware encoder, and the loop that encodes the newest
//! captured picture at the share's rate. Capture and conversion are the platform's (`linux`,
//! `windows`); they hand this module NV12 pictures in the encoder's GPU memory.
//!
//! The encoder settings come from an offline bench (4K60 at 21 Mbps: a still desktop, scrolling
//! text and Big Buck Bunny, on an RX 9060 XT, Linux): an 8-frame VBV. A one-frame buffer made 60-96
//! KB keyframes that stayed blurry for ~0.9 s (1% low VMAF 25-29); FFmpeg's default ~1 s buffer let
//! a still 4K screen key at 1.2 MB, which Chromium's hardware decoder path couldn't absorb (the
//! viewer asked for a keyframe 4x/s, each one as large). 8 frames: keyframes ~370-515 KB, VMAF 95.9
//! on video (GStreamer's `vah264enc` CBR: 91.8), the rate on target. Keyframes only on request, no
//! B-frames, and bitrate changes applied to the next frame without a keyframe (our FFmpeg patches,
//! `scripts/deps/ffmpeg-*.patch`).

use std::ffi::{CStr, CString, c_int};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use ffmpeg_sys_next as ff;
use serde_json::{Value, json};

use crate::{Codec, Out, event, h264_sps, write_record};

/// An FFmpeg error code as a message.
pub fn averr(code: c_int) -> String {
    let mut buf = [0 as std::ffi::c_char; 128];
    unsafe { ff::av_strerror(code, buf.as_mut_ptr(), buf.len()) };
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

pub fn check(code: c_int, what: &str) -> Result<c_int> {
    if code < 0 { Err(anyhow!("{what}: {}", averr(code))) } else { Ok(code) }
}

pub fn cstr(s: &str) -> CString {
    CString::new(s).expect("no NUL in FFmpeg strings")
}

/// The encode API: Linux's Vulkan video and VA-API, Windows' vendor encoders.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(windows, allow(dead_code))]
pub enum Api {
    Vulkan,
    Vaapi,
    Amf,
    Nvenc,
    Qsv,
}

impl Api {
    pub fn name(self) -> &'static str {
        match self {
            Api::Vulkan => "vulkan",
            Api::Vaapi => "vaapi",
            Api::Amf => "amf",
            Api::Nvenc => "nvenc",
            Api::Qsv => "qsv",
        }
    }

    fn encoder(self, codec: Codec) -> String {
        let c = match codec {
            Codec::H264 => "h264",
            Codec::Av1 => "av1",
        };
        format!("{c}_{}", self.name())
    }
}

/// An owned `AVBufferRef` (devices and frame pools).
pub struct BufRef(pub *mut ff::AVBufferRef);

unsafe impl Send for BufRef {}
unsafe impl Sync for BufRef {}

impl BufRef {
    pub fn as_ptr(&self) -> *mut ff::AVBufferRef {
        self.0
    }
    pub fn new_ref(&self) -> *mut ff::AVBufferRef {
        unsafe { ff::av_buffer_ref(self.0) }
    }
}

impl Drop for BufRef {
    fn drop(&mut self) {
        unsafe { ff::av_buffer_unref(&mut self.0) }
    }
}

/// An owned `AVFrame`.
pub struct Frame(pub *mut ff::AVFrame);

unsafe impl Send for Frame {}

impl Frame {
    pub fn alloc() -> Frame {
        Frame(unsafe { ff::av_frame_alloc() })
    }
    /// A new reference to the same picture (no copy).
    pub fn share(&self) -> Frame {
        Frame(unsafe { ff::av_frame_clone(self.0) })
    }
    pub fn size(&self) -> (u32, u32) {
        unsafe { ((*self.0).width as u32, (*self.0).height as u32) }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { ff::av_frame_free(&mut self.0) }
    }
}

pub struct Settings {
    pub codec: Codec,
    pub size: (u32, u32),
    pub fps: u32,
    pub kbps: u32,
}

pub struct Encoder {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    pub name: String,
    api: Api,
    fps: u32,
}

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn open(api: Api, frames: *mut ff::AVBufferRef, s: &Settings) -> Result<Encoder> {
        let name = api.encoder(s.codec);
        unsafe {
            let codec = ff::avcodec_find_encoder_by_name(cstr(&name).as_ptr());
            if codec.is_null() {
                bail!("{name} is not in this FFmpeg");
            }
            let ctx = ff::avcodec_alloc_context3(codec);
            let me = Encoder { ctx, pkt: ff::av_packet_alloc(), name: name.clone(), api, fps: s.fps };
            let fc = (*frames).data as *mut ff::AVHWFramesContext;
            (*ctx).width = s.size.0 as c_int;
            (*ctx).height = s.size.1 as c_int;
            (*ctx).time_base = ff::AVRational { num: 1, den: s.fps as c_int };
            (*ctx).framerate = ff::AVRational { num: s.fps as c_int, den: 1 };
            (*ctx).pix_fmt = (*fc).format;
            (*ctx).sw_pix_fmt = (*fc).sw_format;
            // BT.709, limited range: what the converters produce and what the stream says
            (*ctx).colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
            (*ctx).color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
            (*ctx).color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
            (*ctx).color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            (*ctx).hw_frames_ctx = ff::av_buffer_ref(frames);
            // Keyframes only on request (a viewer joining or losing packets asks with a PLI): a
            // scheduled one is a burst for nothing
            (*ctx).gop_size = i16::MAX as c_int;
            (*ctx).max_b_frames = 0;
            if matches!(api, Api::Amf | Api::Nvenc | Api::Qsv) {
                // Each frame out as soon as it is encoded (AMF otherwise holds one back)
                (*ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as c_int;
            }
            me.set_rate(s.kbps);
            let h264 = s.codec == Codec::H264;
            let mut opts: Vec<(&str, String)> = Vec::new();
            let mut opt = |k: &'static str, v: &str| opts.push((k, v.to_owned()));
            match api {
                // Vulkan: VBR, which a still screen leaves at ~130 kbps; our FFmpeg caps its frames
                // at the VBV (scene cuts stayed under 170 KB at 4K, the rate within 6% of the
                // target). VA-API has no such cap and its VBR ignored the VBV (1.2 MB frames in the
                // bench): CBR, which pads a still screen up to the target
                Api::Vulkan | Api::Vaapi => {
                    opt("rc_mode", if api == Api::Vaapi { "CBR" } else { "vbr" });
                    opt("idr_interval", &i32::MAX.to_string());
                    opt("async_depth", "1");
                    if h264 {
                        // The profile Sharkord negotiates (42e01f): Constrained Baseline, CAVLC
                        opt("profile", "constrained_baseline");
                        opt("coder", if api == Api::Vaapi { "cavlc" } else { "vlc" });
                    }
                    if api == Api::Vulkan {
                        opt("tune", "ull");
                        opt("usage", "stream");
                        opt("content", "desktop");
                    } else if h264 {
                        opt("sei", "0");
                    }
                }
                // AMF: latency-constrained VBR under the VBV, which HRD enforcement makes the frame
                // size cap too; real-time mode, one frame in flight, an IDR (with SPS/PPS or the
                // sequence header) for every forced keyframe
                Api::Amf => {
                    opt("usage", "ultralowlatency");
                    opt("rc", "vbr_latency");
                    opt("enforce_hrd", "1");
                    opt("filler_data", "0");
                    opt(if h264 { "frame_skipping" } else { "skip_frame" }, "0");
                    opt("forced_idr", "1");
                    opt("async_depth", "1");
                    if h264 {
                        opt("latency", "1");
                        opt("profile", "constrained_baseline");
                        opt("coder", "cavlc");
                    } else {
                        opt("latency", "lowest_latency");
                        opt("header_insertion_mode", "gop");
                    }
                }
                // NVENC: ultra-low-latency VBR, no scene-cut keyframes, forced keyframes as IDR
                Api::Nvenc => {
                    opt("preset", "p4");
                    opt("tune", "ull");
                    opt("rc", "vbr");
                    opt("multipass", "disabled");
                    opt("zerolatency", "1");
                    opt("delay", "0");
                    opt("no-scenecut", "1");
                    opt("forced-idr", "1");
                    if h264 {
                        opt("profile", "baseline");
                        opt("coder", "cavlc");
                    }
                }
                // Quick Sync: VBR with its low-delay rate control (frame sizes held to the VBV),
                // one frame in flight, forced keyframes as IDR
                Api::Qsv => {
                    opt("async_depth", "1");
                    opt("low_delay_brc", "1");
                    opt("forced_idr", "1");
                    opt("look_ahead", "0");
                    if h264 {
                        opt("profile", "baseline");
                        opt("cavlc", "1");
                    }
                }
            }
            for (k, v) in &opts {
                let r = ff::av_opt_set((*ctx).priv_data, cstr(k).as_ptr(), cstr(v).as_ptr(), 0);
                if r < 0 {
                    eprintln!("{name}: option {k}={v}: {}", averr(r));
                }
            }
            if api == Api::Amf {
                let r = ff::avcodec_open2(ctx, codec, ptr::null_mut());
                if r >= 0 {
                    return Ok(me);
                }
                // Some drivers reject the ultra-low-latency mode (AMF issue #410, as Sunshine found)
                ff::av_opt_set((*ctx).priv_data, c"usage".as_ptr(), c"lowlatency".as_ptr(), 0);
            }
            check(ff::avcodec_open2(ctx, codec, ptr::null_mut()), &format!("open {name}"))?;
            Ok(me)
        }
    }

    /// The target `kbps` (the peak; VBR's average a little under it), with 8 frames of it as the
    /// VBV (see the module comment).
    pub fn set_rate(&self, kbps: u32) {
        let bps = i64::from(kbps.max(100)) * 1000;
        let vbv = bps * 8 / i64::from(self.fps.max(1));
        unsafe {
            // VBR ran ~6% over its average on Vulkan: aimed at 94%, it lands on the target (sending
            // over Chromium's estimate queued 200-500 ms when motion resumed)
            (*self.ctx).bit_rate = if self.api == Api::Vaapi { bps } else { bps * 94 / 100 };
            (*self.ctx).rc_max_rate = bps;
            (*self.ctx).rc_buffer_size = vbv as c_int;
            if self.api == Api::Qsv {
                // Quick Sync's frame cap, in bytes: the VBV (read on every frame)
                let cap = cstr(&(vbv / 8).to_string());
                ff::av_opt_set((*self.ctx).priv_data, c"max_frame_size".as_ptr(), cap.as_ptr(), 0);
            }
        }
    }

    /// Encodes `frame` as picture number `index`; `key` forces an IDR.
    pub fn send(&mut self, frame: &Frame, index: i64, key: bool) -> Result<()> {
        unsafe {
            (*frame.0).pts = index;
            (*frame.0).pict_type = if key { ff::AVPictureType::AV_PICTURE_TYPE_I } else { ff::AVPictureType::AV_PICTURE_TYPE_NONE };
            if key {
                (*frame.0).flags |= ff::AV_FRAME_FLAG_KEY as c_int;
            } else {
                (*frame.0).flags &= !(ff::AV_FRAME_FLAG_KEY as c_int);
            }
            check(ff::avcodec_send_frame(self.ctx, frame.0), "encode")?;
        }
        Ok(())
    }

    /// The encoded access units ready so far: (data, keyframe, picture number).
    pub fn receive(&mut self, mut each: impl FnMut(&[u8], bool, i64) -> Result<()>) -> Result<()> {
        loop {
            let ret = unsafe { ff::avcodec_receive_packet(self.ctx, self.pkt) };
            if ret == ff::AVERROR(ff::EAGAIN) || ret == ff::AVERROR_EOF {
                return Ok(());
            }
            check(ret, "encoded frame")?;
            let (data, key, pts) = unsafe {
                let p = &*self.pkt;
                (std::slice::from_raw_parts(p.data, p.size as usize), p.flags & ff::AV_PKT_FLAG_KEY as c_int != 0, p.pts)
            };
            let r = each(data, key, pts);
            unsafe { ff::av_packet_unref(self.pkt) };
            r?;
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_packet_free(&mut self.pkt);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}

/// Encodes one small picture with `codec`: the probe's proof that the whole path works here.
pub fn can_encode(api: Api, codec: Codec, frame: &Frame) -> bool {
    let attempt = || -> Result<()> {
        let frames = unsafe { (*frame.0).hw_frames_ctx };
        let mut enc = Encoder::open(api, frames, &Settings { codec, size: frame.size(), fps: 60, kbps: 2000 })?;
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

/// The newest converted picture, from the capture thread to the encoder thread.
pub struct Latest {
    pub frame: Option<Frame>,
    pub fresh: bool,
    /// The captured size, and what the capture looks like (the `input` event)
    pub source: (u32, u32),
    pub input: Option<Value>,
}

/// What the capture thread and the encoder thread share.
pub struct Shared {
    pub latest: Mutex<Latest>,
    pub failed: Mutex<Option<String>>,
    pub captured: AtomicU64,
}

impl Shared {
    pub fn new() -> Shared {
        Shared {
            latest: Mutex::new(Latest { frame: None, fresh: false, source: (0, 0), input: None }),
            failed: Mutex::new(None),
            captured: AtomicU64::new(0),
        }
    }

    /// A converted picture from the capture thread.
    pub fn put(&self, frame: Frame, source: (u32, u32), input: Option<Value>) {
        *self.latest.lock().expect("latest") = Latest { frame: Some(frame), fresh: true, source, input };
        self.captured.fetch_add(1, Ordering::Relaxed);
    }

    pub fn fail(&self, e: &anyhow::Error) {
        self.failed.lock().expect("failed").get_or_insert(format!("{e:#}"));
    }
}

/// The share's encoder side: what `run` needs besides the pictures.
pub struct Pacing<'a> {
    pub api: Api,
    pub codec: Codec,
    pub fps: u32,
    pub kbps: &'a AtomicU32,
    pub keyframe: &'a AtomicBool,
    /// Fields of the `started` event besides the encoder and sizes (the device, the monitor)
    pub started: Value,
}

/// Encodes at the share's rate until `stop` (or the capture fails).
///
/// The encoder ticks at the share's rate and repeats the last picture while the screen is still
/// (compositors and DXGI only send on damage). Frames are stamped by this tick, so the capture's
/// clock never meets ours, and a busy encoder skips a tick instead of queueing.
pub fn run(p: &Pacing, shared: &Shared, out: &Out, stop: &AtomicBool) -> Result<()> {
    let fps = p.fps.max(1);
    let period = Duration::from_nanos(1_000_000_000 / u64::from(fps));
    let mut next = Instant::now();
    let mut enc: Option<(Encoder, (u32, u32))> = None;
    let mut last: Option<Frame> = None;
    let mut kbps = p.kbps.load(Ordering::Relaxed);
    let (mut tick, mut duplicated, mut dropped, mut sent) = (0i64, 0u64, 0u64, 0u64);
    let (mut frames, mut keys, mut bytes, mut since) = (0u64, 0u64, 0u64, Instant::now());
    let mut reported_sps = p.codec != Codec::H264;
    let mut started = false;
    while !stop.load(Ordering::Relaxed) {
        if let Some(e) = shared.failed.lock().expect("failed").take() {
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

        let (frame, source, input) = {
            let mut latest = shared.latest.lock().expect("latest");
            if latest.fresh {
                latest.fresh = false;
                last = latest.frame.as_ref().map(Frame::share);
            } else if last.is_some() {
                duplicated += 1;
            }
            (last.as_ref().map(Frame::share), latest.source, latest.input.clone())
        };
        let Some(frame) = frame else { continue };
        let size = frame.size();

        // A new size (the first frame, or the source changed shape): a new encoder, keyed
        let mut key = p.keyframe.swap(false, Ordering::Relaxed);
        if enc.as_ref().is_none_or(|e| e.1 != size) {
            drop(enc.take());
            let frames = unsafe { (*frame.0).hw_frames_ctx };
            let e = Encoder::open(p.api, frames, &Settings { codec: p.codec, size, fps, kbps })?;
            enc = Some((e, size));
            key = true;
        }
        let (e, _) = enc.as_mut().expect("encoder");
        let wanted = p.kbps.load(Ordering::Relaxed);
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
                let mut ev = json!({
                    "type": "started", "encoder": name,
                    "source": [source.0, source.1], "size": [size.0, size.1], "fps": fps, "api": p.api.name(),
                });
                if let (Some(ev), Some(extra)) = (ev.as_object_mut(), p.started.as_object()) {
                    ev.extend(extra.clone());
                }
                event(out, ev);
                if let Some(input) = &input {
                    event(out, input.clone());
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
            let rate = [shared.captured.load(Ordering::Relaxed), sent, duplicated, dropped];
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

/// The encoder bench the GPU tests share (`cargo test -- --ignored`, on each platform's converter).
#[cfg(test)]
pub mod bench {
    use super::*;

    /// Two 4K pictures in memory (BGRx) to alternate, i.e. motion in every frame: a real desktop
    /// (SHARKORD_TEST_FRAME: raw BGRx at that size, e.g. a screenshot through `ffmpeg -i shot.png
    /// -pix_fmt bgr0 -f rawvideo`) against grey with sparse text-like dots, or two such dot patterns.
    pub fn pictures(w: u32, h: u32) -> [Frame; 2] {
        let raw = std::env::var("SHARKORD_TEST_FRAME").ok().map(|p| std::fs::read(p).expect("test frame"));
        [1u32, 7].map(|seed| unsafe {
            let f = Frame::alloc();
            (*f.0).format = ff::AVPixelFormat::AV_PIX_FMT_BGR0 as c_int;
            (*f.0).width = w as c_int;
            (*f.0).height = h as c_int;
            assert!(ff::av_frame_get_buffer(f.0, 0) >= 0);
            let stride = (*f.0).linesize[0] as usize;
            let data = std::slice::from_raw_parts_mut((*f.0).data[0], stride * h as usize);
            if let (Some(raw), 1) = (&raw, seed) {
                for y in 0..h as usize {
                    data[y * stride..y * stride + w as usize * 4].copy_from_slice(&raw[y * w as usize * 4..(y + 1) * w as usize * 4]);
                }
            } else {
                let mut x = seed;
                for (i, b) in data.iter_mut().enumerate() {
                    x ^= x << 13; x ^= x >> 17; x ^= x << 5;
                    *b = if (i / stride) % 24 < 12 && x & 31 == 0 { 230 } else { 40 };
                }
            }
            f
        })
    }

    /// Encodes 3 s of 4K60 H.264 from `picture(i)` and checks what the Linux bench measured: one IDR
    /// at the start and one more only when asked (picture 120), keyframes capped by the 8-frame VBV,
    /// and a bitrate change (21.25 -> 12 Mbps at picture 60) applied without a keyframe.
    pub fn keyframes_on_request(api: Api, mut picture: impl FnMut(usize) -> Frame) {
        let first = picture(0);
        let frames = unsafe { (*first.0).hw_frames_ctx };
        let mut enc = Encoder::open(api, frames, &Settings { codec: Codec::H264, size: first.size(), fps: 60, kbps: 21250 }).unwrap();
        let mut keys = Vec::new();
        let mut bytes = [0usize; 3];
        for i in 0..180 {
            let f = if i == 0 { first.share() } else { picture(i) };
            enc.send(&f, i as i64, i == 0 || i == 120).unwrap();
            if i == 60 {
                enc.set_rate(12000);   // must not key
            }
            enc.receive(|data, key, pts| {
                if key { keys.push((pts, data.len())); } else { bytes[(pts / 60) as usize] += data.len(); }
                Ok(())
            }).unwrap();
        }
        let kbps = bytes.map(|b| b * 8 / 1000);
        eprintln!("{} keyframes (picture, bytes): {keys:?}; kbps per second (P frames): {kbps:?}", enc.name);
        assert_eq!(keys.iter().map(|k| k.0).collect::<Vec<_>>(), vec![0, 120], "{}", enc.name);
        assert!(keys.iter().all(|k| k.1 < 600 * 1024), "{}: keyframe over 600 KB", enc.name);
        assert!(kbps[1] < 14000, "{}: bitrate change not applied", enc.name);
    }
}
