//! FFmpeg: the GPU device, the conversion of captured frames into the encoder's NV12 at the
//! share's size, and the hardware encoder (Vulkan video, or VA-API).
//!
//! The settings come from an offline bench (4K60 at 21 Mbps: a still desktop, scrolling text and
//! Big Buck Bunny, on an RX 9060 XT): CBR with an 8-frame VBV. A one-frame buffer made 60-96 KB
//! keyframes that stayed blurry for ~0.9 s (1% low VMAF 25-29); FFmpeg's default ~1 s buffer let a
//! still 4K screen key at 1.2 MB, which Chromium's hardware decoder path couldn't absorb (the viewer
//! asked for a keyframe 4x/s, each one as large). 8 frames: keyframes ~370-515 KB, VMAF 95.9 on
//! video (GStreamer's `vah264enc` CBR: 91.8), the rate on target. Vulkan and VA-API encode
//! identically on the same GPU; Vulkan comes first because its driver is the one this app can
//! bundle.

use std::ffi::{CStr, CString, c_int};
use std::ptr;

use anyhow::{Result, anyhow, bail};
use ffmpeg_sys_next as ff;

use crate::Codec;

/// An FFmpeg error code as a message.
fn averr(code: c_int) -> String {
    let mut buf = [0 as std::ffi::c_char; 128];
    unsafe { ff::av_strerror(code, buf.as_mut_ptr(), buf.len()) };
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

fn check(code: c_int, what: &str) -> Result<c_int> {
    if code < 0 { Err(anyhow!("{what}: {}", averr(code))) } else { Ok(code) }
}

fn cstr(s: &str) -> CString {
    CString::new(s).expect("no NUL in FFmpeg strings")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Api {
    Vulkan,
    Vaapi,
}

impl Api {
    pub fn name(self) -> &'static str {
        match self {
            Api::Vulkan => "vulkan",
            Api::Vaapi => "vaapi",
        }
    }
}

/// An owned `AVBufferRef` (devices and frame pools).
pub struct BufRef(*mut ff::AVBufferRef);

unsafe impl Send for BufRef {}

impl BufRef {
    pub fn as_ptr(&self) -> *mut ff::AVBufferRef {
        self.0
    }
    fn new_ref(&self) -> *mut ff::AVBufferRef {
        unsafe { ff::av_buffer_ref(self.0) }
    }
}

impl Drop for BufRef {
    fn drop(&mut self) {
        unsafe { ff::av_buffer_unref(&mut self.0) }
    }
}

/// The GPU: a DRM device on a render node (what captured DMA-BUFs belong to), and the encode API
/// derived from it, so both are the same GPU.
pub struct Device {
    pub api: Api,
    pub render_node: String,
    pub drm: BufRef,
    pub hw: BufRef,
}

impl Device {
    pub fn open(render_node: &str, api: Api) -> Result<Device> {
        let node = cstr(render_node);
        let mut drm = ptr::null_mut();
        check(
            unsafe { ff::av_hwdevice_ctx_create(&mut drm, ff::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM, node.as_ptr(), ptr::null_mut(), 0) },
            "DRM device",
        )?;
        let drm = BufRef(drm);
        let kind = match api {
            Api::Vulkan => ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
            Api::Vaapi => ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
        };
        let mut hw = ptr::null_mut();
        check(unsafe { ff::av_hwdevice_ctx_create_derived(&mut hw, kind, drm.as_ptr(), 0) }, &format!("{} device", api.name()))?;
        Ok(Device { api, render_node: render_node.to_owned(), drm, hw: BufRef(hw) })
    }

    /// DRM format modifiers the GPU can sample BGRx/BGRA images with, single-plane only (what a
    /// compositor may render the screen cast into for us to import without a copy). Asked through
    /// Vulkan, from this device or one derived for the purpose; empty if there is no Vulkan.
    pub fn modifiers(&self) -> Vec<u64> {
        use ash::vk::{self, Handle};
        let owned;
        let hw = if self.api == Api::Vulkan {
            &self.hw
        } else {
            let mut vk_dev = ptr::null_mut();
            let ret = unsafe {
                ff::av_hwdevice_ctx_create_derived(&mut vk_dev, ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN, self.drm.as_ptr(), 0)
            };
            if ret < 0 {
                return Vec::new();
            }
            owned = BufRef(vk_dev);
            &owned
        };
        unsafe {
            let ctx = (*(hw.as_ptr())).data as *mut ff::AVHWDeviceContext;
            let vkctx = (*ctx).hwctx as *mut ff::AVVulkanDeviceContext;
            let Some(gipa) = (*vkctx).get_proc_addr else { return Vec::new() };
            let static_fn = ash::StaticFn { get_instance_proc_addr: std::mem::transmute::<_, vk::PFN_vkGetInstanceProcAddr>(gipa) };
            let instance = ash::Instance::load(&static_fn, vk::Instance::from_raw((*vkctx).inst as u64));
            let pd = vk::PhysicalDevice::from_raw((*vkctx).phys_dev as u64);
            let query = |mods: &mut [vk::DrmFormatModifierPropertiesEXT]| {
                let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(mods);
                let mut props = vk::FormatProperties2::default().push_next(&mut list);
                instance.get_physical_device_format_properties2(pd, vk::Format::B8G8R8A8_UNORM, &mut props);
                list.drm_format_modifier_count as usize
            };
            let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); query(&mut [])];
            let n = query(&mut mods);
            mods.truncate(n);
            mods.iter()
                .filter(|m| m.drm_format_modifier_plane_count == 1 && m.drm_format_modifier_tiling_features.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE))
                .map(|m| m.drm_format_modifier)
                .collect()
        }
    }

    /// Render nodes, in the kernel's order (the first is usually the GPU driving the desktop).
    pub fn render_nodes() -> Vec<String> {
        let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")
            .map(|d| d.filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| n.starts_with("renderD")).collect())
            .unwrap_or_default();
        nodes.sort();
        nodes.into_iter().map(|n| format!("/dev/dri/{n}")).collect()
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
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { ff::av_frame_free(&mut self.0) }
    }
}

/// A captured DMA-BUF as a DRM PRIME frame for the converter, no copy. `release` runs when
/// FFmpeg lets go of it (once the GPU has read it): the buffer goes back to the compositor.
pub fn drm_frame<R>(frames: &BufRef, size: (u32, u32), alpha: bool, fd: i32, len: usize, offset: u32, stride: i32, modifier: u64, release: R) -> Result<Frame> {
    unsafe extern "C" fn free<R>(opaque: *mut std::ffi::c_void, data: *mut u8) {
        unsafe {
            drop(Box::from_raw(opaque as *mut R));
            ff::av_free(data as *mut _);
        }
    }
    unsafe {
        let desc = ff::av_mallocz(std::mem::size_of::<ff::AVDRMFrameDescriptor>()) as *mut ff::AVDRMFrameDescriptor;
        if desc.is_null() {
            bail!("out of memory");
        }
        (*desc).nb_objects = 1;
        (*desc).objects[0] = ff::AVDRMObjectDescriptor { fd, size: len, format_modifier: modifier };
        (*desc).nb_layers = 1;
        // DRM fourccs for PipeWire's BGRx / BGRA (little-endian XRGB / ARGB)
        (*desc).layers[0].format = if alpha { 0x3432_5241 } else { 0x3432_5258 };
        (*desc).layers[0].nb_planes = 1;
        (*desc).layers[0].planes[0] = ff::AVDRMPlaneDescriptor { object_index: 0, offset: offset as isize, pitch: stride as isize };
        let frame = Frame::alloc();
        (*frame.0).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
        (*frame.0).width = size.0 as c_int;
        (*frame.0).height = size.1 as c_int;
        (*frame.0).data[0] = desc as *mut u8;
        (*frame.0).buf[0] = ff::av_buffer_create(
            desc as *mut u8,
            std::mem::size_of::<ff::AVDRMFrameDescriptor>(),
            Some(free::<R>),
            Box::into_raw(Box::new(release)) as *mut _,
            0,
        );
        (*frame.0).hw_frames_ctx = frames.new_ref();
        Ok(frame)
    }
}

/// A frame in shared memory, copied (the compositor gets the buffer back at once).
pub fn memory_frame(size: (u32, u32), alpha: bool, data: &[u8], stride: usize) -> Result<Frame> {
    unsafe {
        let frame = Frame::alloc();
        (*frame.0).format = (if alpha { ff::AVPixelFormat::AV_PIX_FMT_BGRA } else { ff::AVPixelFormat::AV_PIX_FMT_BGR0 }) as c_int;
        (*frame.0).width = size.0 as c_int;
        (*frame.0).height = size.1 as c_int;
        check(ff::av_frame_get_buffer(frame.0, 0), "frame")?;
        let (row, dst_stride) = (size.0 as usize * 4, (*frame.0).linesize[0] as usize);
        for y in 0..size.1 as usize {
            let src = data.get(y * stride..y * stride + row).ok_or_else(|| anyhow!("short frame"))?;
            std::ptr::copy_nonoverlapping(src.as_ptr(), (*frame.0).data[0].add(y * dst_stride), row);
        }
        Ok(frame)
    }
}

/// What the converter is fed.
pub enum Input {
    /// DMA-BUFs from PipeWire, wrapped as DRM PRIME frames: mapped into the encode API, no copy
    Drm,
    /// Frames in memory (PipeWire without DMA-BUF): uploaded
    Memory,
}

/// Captured frames (BGRx/BGRA) → NV12 at the share's size, in the encoder's GPU memory.
pub struct Convert {
    graph: *mut ff::AVFilterGraph,
    src: *mut ff::AVFilterContext,
    sink: *mut ff::AVFilterContext,
    /// DRM input: the frame pool the captured frames claim to come from (hwmap needs one)
    pub drm_frames: Option<BufRef>,
}

unsafe impl Send for Convert {}

impl Convert {
    pub fn new(dev: &Device, input: Input, format: ff::AVPixelFormat, size: (u32, u32), out: (u32, u32)) -> Result<Convert> {
        unsafe {
            let graph = ff::avfilter_graph_alloc();
            if graph.is_null() {
                bail!("filter graph");
            }
            let mut me = Convert { graph, src: ptr::null_mut(), sink: ptr::null_mut(), drm_frames: None };
            let in_fmt = match input {
                Input::Drm => ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME,
                Input::Memory => format,
            };
            let args = cstr(&format!("video_size={}x{}:pix_fmt={}:time_base=1/1000000:pixel_aspect=1/1", size.0, size.1, in_fmt as i32));
            check(
                ff::avfilter_graph_create_filter(&mut me.src, ff::avfilter_get_by_name(c"buffer".as_ptr()), c"in".as_ptr(), args.as_ptr(), ptr::null_mut(), graph),
                "buffer source",
            )?;
            if let Input::Drm = input {
                let frames = ff::av_hwframe_ctx_alloc(dev.drm.as_ptr());
                let fc = (*frames).data as *mut ff::AVHWFramesContext;
                (*fc).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME;
                (*fc).sw_format = format;
                (*fc).width = size.0 as c_int;
                (*fc).height = size.1 as c_int;
                let frames = BufRef(frames);
                check(ff::av_hwframe_ctx_init(frames.as_ptr()), "DRM frame pool")?;
                let par = ff::av_buffersrc_parameters_alloc();
                (*par).hw_frames_ctx = frames.as_ptr();
                let ret = ff::av_buffersrc_parameters_set(me.src, par);
                ff::av_free(par as *mut _);
                check(ret, "buffer source frames")?;
                me.drm_frames = Some(frames);
            }
            check(
                ff::avfilter_graph_create_filter(&mut me.sink, ff::avfilter_get_by_name(c"buffersink".as_ptr()), c"out".as_ptr(), ptr::null(), ptr::null_mut(), graph),
                "buffer sink",
            )?;
            let (w, h) = out;
            let chain = match (dev.api, &input) {
                (Api::Vulkan, Input::Drm) => format!("hwmap=derive_device=vulkan,scale_vulkan=w={w}:h={h}:format=nv12"),
                (Api::Vulkan, Input::Memory) => format!("scale_vulkan=w={w}:h={h}:format=nv12"),
                (Api::Vaapi, Input::Drm) => format!("hwmap=derive_device=vaapi,scale_vaapi=w={w}:h={h}:format=nv12"),
                (Api::Vaapi, Input::Memory) => format!("scale_vaapi=w={w}:h={h}:format=nv12"),
            };
            // Memory input: upload first. hwupload needs its device when it is created, which a
            // parsed graph can't give it, so it is made here and the rest is parsed after it
            let mut head = me.src;
            if let Input::Memory = input {
                let up = ff::avfilter_graph_alloc_filter(graph, ff::avfilter_get_by_name(c"hwupload".as_ptr()), c"upload".as_ptr());
                (*up).hw_device_ctx = dev.hw.new_ref();
                check(ff::avfilter_init_str(up, ptr::null()), "hwupload")?;
                check(ff::avfilter_link(me.src, 0, up, 0), "hwupload link")?;
                head = up;
            }
            let mut outputs = ff::avfilter_inout_alloc();
            let mut inputs = ff::avfilter_inout_alloc();
            (*outputs).name = ff::av_strdup(c"in".as_ptr());
            (*outputs).filter_ctx = head;
            (*inputs).name = ff::av_strdup(c"out".as_ptr());
            (*inputs).filter_ctx = me.sink;
            let spec = cstr(&chain);
            let ret = ff::avfilter_graph_parse_ptr(graph, spec.as_ptr(), &mut inputs, &mut outputs, ptr::null_mut());
            ff::avfilter_inout_free(&mut inputs);
            ff::avfilter_inout_free(&mut outputs);
            check(ret, &format!("filter graph {chain}"))?;
            check(ff::avfilter_graph_config(graph, ptr::null_mut()), &format!("filter graph {chain}"))?;
            Ok(me)
        }
    }

    /// Converts one captured frame; the result is a new picture (the captured buffer can go back to
    /// the compositor once the GPU has read it).
    pub fn convert(&mut self, frame: &Frame) -> Result<Frame> {
        unsafe {
            check(ff::av_buffersrc_add_frame_flags(self.src, frame.0, ff::AV_BUFFERSRC_FLAG_KEEP_REF as c_int), "convert")?;
            let out = Frame::alloc();
            check(ff::av_buffersink_get_frame(self.sink, out.0), "convert output")?;
            Ok(out)
        }
    }

    /// The pool the converted frames come from: the encoder allocates from its description.
    #[cfg(test)]
    pub fn frames(&self) -> *mut ff::AVBufferRef {
        unsafe { ff::av_buffersink_get_hw_frames_ctx(self.sink) }
    }
}

impl Drop for Convert {
    fn drop(&mut self) {
        unsafe { ff::avfilter_graph_free(&mut self.graph) }
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
    fps: u32,
}

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn open(dev: &Device, frames: *mut ff::AVBufferRef, s: &Settings) -> Result<Encoder> {
        let name = match (dev.api, s.codec) {
            (Api::Vulkan, Codec::H264) => "h264_vulkan",
            (Api::Vulkan, Codec::Av1) => "av1_vulkan",
            (Api::Vaapi, Codec::H264) => "h264_vaapi",
            (Api::Vaapi, Codec::Av1) => "av1_vaapi",
        };
        unsafe {
            let codec = ff::avcodec_find_encoder_by_name(cstr(name).as_ptr());
            if codec.is_null() {
                bail!("{name} is not in this FFmpeg");
            }
            let ctx = ff::avcodec_alloc_context3(codec);
            let me = Encoder { ctx, pkt: ff::av_packet_alloc(), name: name.to_owned(), fps: s.fps };
            (*ctx).width = s.size.0 as c_int;
            (*ctx).height = s.size.1 as c_int;
            (*ctx).time_base = ff::AVRational { num: 1, den: s.fps as c_int };
            (*ctx).framerate = ff::AVRational { num: s.fps as c_int, den: 1 };
            (*ctx).pix_fmt = (*((*frames).data as *mut ff::AVHWFramesContext)).format;
            (*ctx).sw_pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_NV12;
            (*ctx).hw_frames_ctx = ff::av_buffer_ref(frames);
            // Keyframes only on request (a viewer joining or losing packets asks with a PLI): a
            // scheduled one is a burst for nothing
            (*ctx).gop_size = i16::MAX as c_int;
            (*ctx).max_b_frames = 0;
            me.set_rate(s.kbps);
            let opt = |k: &str, v: &str| ff::av_opt_set((*ctx).priv_data, cstr(k).as_ptr(), cstr(v).as_ptr(), 0);
            opt("rc_mode", if dev.api == Api::Vaapi { "CBR" } else { "cbr" });
            opt("idr_interval", &i32::MAX.to_string());
            opt("async_depth", "1");
            if s.codec == Codec::H264 {
                // The profile Sharkord negotiates (42e01f): Constrained Baseline, CAVLC
                opt("profile", "constrained_baseline");
                opt("coder", if dev.api == Api::Vaapi { "cavlc" } else { "vlc" });
            }
            if dev.api == Api::Vulkan {
                opt("tune", "ull");
                opt("usage", "stream");
                opt("content", "desktop");
            } else if s.codec == Codec::H264 {
                opt("sei", "0");
            }
            check(ff::avcodec_open2(ctx, codec, ptr::null_mut()), &format!("open {name}"))?;
            Ok(me)
        }
    }

    /// CBR at `kbps`, with 8 frames of it as the VBV (see the module comment).
    pub fn set_rate(&self, kbps: u32) {
        let bps = i64::from(kbps.max(100)) * 1000;
        unsafe {
            (*self.ctx).bit_rate = bps;
            (*self.ctx).rc_max_rate = bps;
            (*self.ctx).rc_buffer_size = (bps * 8 / i64::from(self.fps.max(1))) as c_int;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a GPU that encodes H.264: `cargo test -- --ignored` (SHARKORD_TEST_FRAME: a real
    /// desktop). Encodes 3 s of a still 4K picture through the memory path on Vulkan and VA-API and
    /// checks what the bench measured: one IDR at the start, one more only when asked, keyframes
    /// capped by the 8-frame VBV, and a bitrate change applied without a keyframe.
    #[test]
    #[ignore]
    fn encodes_4k_with_keyframes_only_on_request() {
        for api in [Api::Vulkan, Api::Vaapi] {
            let dev = Device::render_nodes().iter().find_map(|n| Device::open(n, api).ok()).expect("a GPU");
            let (w, h) = (3840u32, 2160u32);
            let mut conv = Convert::new(&dev, Input::Memory, ff::AVPixelFormat::AV_PIX_FMT_BGR0, (w, h), (w, h)).unwrap();
            let src = Frame::alloc();
            unsafe {
                (*src.0).format = ff::AVPixelFormat::AV_PIX_FMT_BGR0 as c_int;
                (*src.0).width = w as c_int;
                (*src.0).height = h as c_int;
                assert!(ff::av_frame_get_buffer(src.0, 0) >= 0);
                // A real desktop (SHARKORD_TEST_FRAME: raw 3840x2160 BGRx, e.g. a screenshot through
                // `ffmpeg -i shot.png -pix_fmt bgr0 -f rawvideo`), else flat grey with sparse text-like dots
                let stride = (*src.0).linesize[0] as usize;
                let data = std::slice::from_raw_parts_mut((*src.0).data[0], stride * h as usize);
                if let Ok(raw) = std::env::var("SHARKORD_TEST_FRAME").map(|p| std::fs::read(p).expect("test frame")) {
                    for y in 0..h as usize {
                        data[y * stride..y * stride + w as usize * 4].copy_from_slice(&raw[y * w as usize * 4..(y + 1) * w as usize * 4]);
                    }
                } else {
                    let mut x: u32 = 1;
                    for (i, b) in data.iter_mut().enumerate() {
                        x ^= x << 13; x ^= x >> 17; x ^= x << 5;
                        *b = if (i / stride) % 24 < 12 && x & 31 == 0 { 230 } else { 40 };
                    }
                }
            }
            let first = conv.convert(&src).unwrap();
            let mut enc = Encoder::open(&dev, conv.frames(), &Settings { codec: Codec::H264, size: (w, h), fps: 60, kbps: 21250 }).unwrap();
            let mut keys = Vec::new();
            let mut bytes = [0usize; 3];
            for i in 0..180 {
                let f = if i == 0 { first.share() } else { conv.convert(&src).unwrap() };
                enc.send(&f, i, i == 0 || i == 120).unwrap();
                if i == 60 {
                    enc.set_rate(12000);   // must not key
                }
                enc.receive(|data, key, pts| {
                    if key { keys.push((pts, data.len())); } else { bytes[(pts / 60) as usize] += data.len(); }
                    Ok(())
                }).unwrap();
            }
            eprintln!("{} keyframes (picture, bytes): {keys:?}; kbps per second (P frames): {:?}", enc.name, bytes.map(|b| b * 8 / 1000));
            assert_eq!(keys.iter().map(|k| k.0).collect::<Vec<_>>(), vec![0, 120], "{}", enc.name);
            assert!(keys.iter().all(|k| k.1 < 600 * 1024), "{}: keyframe over 600 KB", enc.name);
            // 21.25 -> 12 Mbps at picture 60 (our FFmpeg patches): the next second follows it
            assert!(bytes[1] * 8 / 1000 < 14000, "{}: bitrate change not applied", enc.name);
        }
    }
}
