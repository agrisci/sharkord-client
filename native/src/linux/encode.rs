//! FFmpeg on Linux: the GPU device, and the conversion of captured frames into the encoder's NV12
//! at the share's size (the encoder is `crate::ffmpeg`'s). Vulkan and VA-API encode identically on
//! the same GPU; Vulkan comes first because its driver is the one this app can bundle.

use std::ffi::c_int;
use std::ptr;

use anyhow::{Result, anyhow, bail};
use ffmpeg_sys_next as ff;

pub use crate::ffmpeg::{Api, BufRef, Frame};
use crate::ffmpeg::{check, cstr};

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
            // The shared `Api` also lists Windows' encoders
            _ => bail!("{} is not a Linux encode API", api.name()),
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
            // First the count (a null array: an empty one would be read as room for none), then the list
            let mut count = vk::DrmFormatModifierPropertiesListEXT::default();
            let mut props = vk::FormatProperties2::default().push_next(&mut count);
            instance.get_physical_device_format_properties2(pd, vk::Format::B8G8R8A8_UNORM, &mut props);
            let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); count.drm_format_modifier_count as usize];
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut mods);
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            instance.get_physical_device_format_properties2(pd, vk::Format::B8G8R8A8_UNORM, &mut props);
            let n = list.drm_format_modifier_count as usize;
            mods.truncate(n);
            mods.iter()
                .filter(|m| m.drm_format_modifier_plane_count == 1 && m.drm_format_modifier_tiling_features.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE))
                .map(|m| m.drm_format_modifier)
                .collect()
        }
    }

    /// Waits until the GPU has finished writing `frame` (a converted picture): by then it has read
    /// the captured buffer it came from, which can go back to the compositor. FFmpeg's own release
    /// of that buffer comes only when its filter reuses the work slot, later than the compositor's
    /// few buffers last (KWin lends 3: capture stopped after 3 frames).
    pub fn wait(&self, frame: &Frame) -> Result<()> {
        use ash::vk::{self, Handle};
        unsafe extern "C" {
            fn vaSyncSurface(dpy: *mut std::ffi::c_void, surface: u32) -> c_int;
        }
        unsafe {
            let ctx = (*(self.hw.as_ptr())).data as *mut ff::AVHWDeviceContext;
            match self.api {
                Api::Vulkan => {
                    let vkctx = (*ctx).hwctx as *mut ff::AVVulkanDeviceContext;
                    let Some(gipa) = (*vkctx).get_proc_addr else { bail!("no Vulkan loader") };
                    let static_fn = ash::StaticFn { get_instance_proc_addr: std::mem::transmute::<_, vk::PFN_vkGetInstanceProcAddr>(gipa) };
                    let instance = ash::Instance::load(&static_fn, vk::Instance::from_raw((*vkctx).inst as u64));
                    let device = ash::Device::load(instance.fp_v1_0(), vk::Device::from_raw((*vkctx).act_dev as u64));
                    let vkf = (*frame.0).data[0] as *const ff::AVVkFrame;
                    let (mut sems, mut values) = (Vec::new(), Vec::new());
                    for i in 0..8 {
                        if (*vkf).img[i] == 0 {
                            break;
                        }
                        sems.push(vk::Semaphore::from_raw((*vkf).sem[i] as u64));
                        values.push((*vkf).sem_value[i]);
                    }
                    device
                        .wait_semaphores(&vk::SemaphoreWaitInfo::default().semaphores(&sems).values(&values), 1_000_000_000)
                        .map_err(|e| anyhow!("waiting for the conversion: {e}"))?;
                }
                Api::Vaapi => {
                    let va = (*ctx).hwctx as *mut ff::AVVAAPIDeviceContext;
                    let r = vaSyncSurface((*va).display as *mut _, (*frame.0).data[3] as usize as u32);
                    if r != 0 {
                        bail!("waiting for the conversion: VA error {r}");
                    }
                }
                _ => unreachable!("a Linux device is Vulkan or VA-API"),
            }
        }
        Ok(())
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

/// What a captured screen is: full-range RGB, converted with BT.709 (the scaler needs to be told;
/// the encoder signals the same in the stream).
fn rgb_screen(frame: &Frame) {
    unsafe {
        (*frame.0).colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
        (*frame.0).color_range = ff::AVColorRange::AVCOL_RANGE_JPEG;
        (*frame.0).color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
        (*frame.0).color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_IEC61966_2_1;
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
        rgb_screen(&frame);
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
        rgb_screen(&frame);
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
            // The source is described before it is initialized: a DRM PRIME one needs its frame pool then
            me.src = ff::avfilter_graph_alloc_filter(graph, ff::avfilter_get_by_name(c"buffer".as_ptr()), c"in".as_ptr());
            if me.src.is_null() {
                bail!("buffer source");
            }
            let par = ff::av_buffersrc_parameters_alloc();
            (*par).format = in_fmt as c_int;
            (*par).width = size.0 as c_int;
            (*par).height = size.1 as c_int;
            (*par).time_base = ff::AVRational { num: 1, den: 1_000_000 };
            (*par).sample_aspect_ratio = ff::AVRational { num: 1, den: 1 };
            if let Input::Drm = input {
                let frames = ff::av_hwframe_ctx_alloc(dev.drm.as_ptr());
                let fc = (*frames).data as *mut ff::AVHWFramesContext;
                (*fc).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME;
                (*fc).sw_format = format;
                (*fc).width = size.0 as c_int;
                (*fc).height = size.1 as c_int;
                let frames = BufRef(frames);
                check(ff::av_hwframe_ctx_init(frames.as_ptr()), "DRM frame pool")?;
                (*par).hw_frames_ctx = frames.as_ptr();
                me.drm_frames = Some(frames);
            }
            (*par).color_space = ff::AVColorSpace::AVCOL_SPC_BT709;
            (*par).color_range = ff::AVColorRange::AVCOL_RANGE_JPEG;
            let ret = ff::av_buffersrc_parameters_set(me.src, par);
            ff::av_free(par as *mut _);
            check(ret, "buffer source parameters")?;
            check(ff::avfilter_init_str(me.src, ptr::null()), "buffer source")?;
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
                _ => unreachable!("a Linux device is Vulkan or VA-API"),
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
}

impl Drop for Convert {
    fn drop(&mut self) {
        unsafe { ff::avfilter_graph_free(&mut self.graph) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a GPU that encodes H.264: `cargo test -- --ignored`. The bench of `crate::ffmpeg`
    /// through the memory path, on Vulkan and VA-API.
    #[test]
    #[ignore]
    fn encodes_4k_with_keyframes_only_on_request() {
        for api in [Api::Vulkan, Api::Vaapi] {
            let dev = Device::render_nodes().iter().find_map(|n| Device::open(n, api).ok()).expect("a GPU");
            let (w, h) = (3840u32, 2160u32);
            let mut conv = Convert::new(&dev, Input::Memory, ff::AVPixelFormat::AV_PIX_FMT_BGR0, (w, h), (w, h)).unwrap();
            let pictures = crate::ffmpeg::bench::pictures(w, h);
            crate::ffmpeg::bench::keyframes_on_request(api, |i| conv.convert(&pictures[i % 2]).unwrap());
        }
    }
}
