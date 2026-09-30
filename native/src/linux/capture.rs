//! The portal's PipeWire stream: negotiates DMA-BUF frames the GPU can import (shared memory when
//! the compositor offers nothing else) and hands the newest frame to the share.
//!
//! Measured on KWin (Renoir and RX 9060 XT) with the old GStreamer graph, and kept here: the
//! compositor lends only a few buffers and stops sending while they are held, so every buffer
//! except the newest goes back at once, and the newest as soon as the GPU has read it.
//! Negotiation follows Sunshine (`pipewire.cpp`) and PipeWire's DMA-BUF protocol: offer the
//! modifiers we can import with DONT_FIXATE, then fixate the compositor's pick.

use std::io::Cursor;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value, serialize::PodSerializer};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::video::VideoFormat;
use spa::param::ParamType;

/// What the compositor agreed to send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub size: (u32, u32),
    /// BGRx or BGRA
    pub alpha: bool,
    /// DMA-BUF with this modifier, or shared memory
    pub modifier: Option<u64>,
}

/// One captured frame. Dropping it gives the buffer back to the compositor.
pub struct Captured {
    pub format: Format,
    pub data: Data,
    release: Release,
}

pub enum Data {
    Dmabuf { fd: i32, size: usize, offset: u32, stride: i32 },
    Memory { ptr: *const u8, len: usize, stride: i32 },
}

impl Captured {
    /// Keeps the buffer out of the compositor's pool until the returned guard drops.
    pub fn into_release(self) -> Release {
        self.release
    }
}

/// Returns a PipeWire buffer to its stream when dropped (on the capture thread: the converter
/// that holds it only runs there).
pub struct Release {
    stream: *mut pw::sys::pw_stream,
    buffer: *mut pw::sys::pw_buffer,
}

impl Drop for Release {
    fn drop(&mut self) {
        unsafe { pw::sys::pw_stream_queue_buffer(self.stream, self.buffer) };
    }
}

fn serialize(value: Value) -> Vec<u8> {
    PodSerializer::serialize(Cursor::new(Vec::new()), &value).expect("pod").0.into_inner()
}

fn prop(key: FormatProperties, value: Value) -> Property {
    Property { key: key.as_raw(), flags: PropertyFlags::empty(), value }
}

/// An EnumFormat: BGRx/BGRA, any size, up to `fps`, with these modifiers (DMA-BUF) or none
/// (shared memory). `fixed`: one modifier, to fixate the compositor's pick.
fn format_pod(fps: u32, modifiers: Option<&[u64]>, fixed: Option<Format>) -> Vec<u8> {
    let mut properties = vec![
        prop(FormatProperties::MediaType, Value::Id(Id(MediaType::Video.as_raw()))),
        prop(FormatProperties::MediaSubtype, Value::Id(Id(MediaSubtype::Raw.as_raw()))),
    ];
    match fixed {
        Some(f) => {
            let format = if f.alpha { VideoFormat::BGRA } else { VideoFormat::BGRx };
            properties.push(prop(FormatProperties::VideoFormat, Value::Id(Id(format.as_raw()))));
            properties.push(prop(FormatProperties::VideoSize, Value::Rectangle(Rectangle { width: f.size.0, height: f.size.1 })));
        }
        None => {
            properties.push(prop(
                FormatProperties::VideoFormat,
                Value::Choice(ChoiceValue::Id(Choice(ChoiceFlags::empty(), ChoiceEnum::Enum {
                    default: Id(VideoFormat::BGRx.as_raw()),
                    alternatives: vec![Id(VideoFormat::BGRx.as_raw()), Id(VideoFormat::BGRA.as_raw())],
                }))),
            ));
            properties.push(prop(
                FormatProperties::VideoSize,
                Value::Choice(ChoiceValue::Rectangle(Choice(ChoiceFlags::empty(), ChoiceEnum::Range {
                    default: Rectangle { width: 1920, height: 1080 },
                    min: Rectangle { width: 1, height: 1 },
                    max: Rectangle { width: 16384, height: 16384 },
                }))),
            ));
        }
    }
    // Variable rate (compositors send on damage: 0/1), or any fixed one, capped at the share's
    properties.push(prop(
        FormatProperties::VideoFramerate,
        Value::Choice(ChoiceValue::Fraction(Choice(ChoiceFlags::empty(), ChoiceEnum::Range {
            default: Fraction { num: 0, denom: 1 },
            min: Fraction { num: 0, denom: 1 },
            max: Fraction { num: 1000, denom: 1 },
        }))),
    ));
    properties.push(prop(
        FormatProperties::VideoMaxFramerate,
        Value::Choice(ChoiceValue::Fraction(Choice(ChoiceFlags::empty(), ChoiceEnum::Range {
            default: Fraction { num: fps, denom: 1 },
            min: Fraction { num: 0, denom: 1 },
            max: Fraction { num: 1000, denom: 1 },
        }))),
    ));
    if let Some(f) = fixed.and_then(|f| f.modifier) {
        properties.push(Property { key: FormatProperties::VideoModifier.as_raw(), flags: PropertyFlags::MANDATORY, value: Value::Long(f as i64) });
    } else if let Some(mods) = modifiers.filter(|m| !m.is_empty()) {
        properties.push(Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY | PropertyFlags::DONT_FIXATE,
            value: Value::Choice(ChoiceValue::Long(Choice(ChoiceFlags::empty(), ChoiceEnum::Enum {
                default: mods[0] as i64,
                alternatives: mods.iter().map(|&m| m as i64).collect(),
            }))),
        });
    }
    serialize(Value::Object(Object { type_: SpaTypes::ObjectParamFormat.as_raw(), id: ParamType::EnumFormat.as_raw(), properties }))
}

/// Buffers of DMA-BUFs (or memory) and the header meta that marks corrupted frames.
fn buffers_pods(dmabuf: bool) -> [Vec<u8>; 2] {
    let data_type = 1i32 << if dmabuf { spa::sys::SPA_DATA_DmaBuf } else { spa::sys::SPA_DATA_MemPtr };
    let buffers = serialize(Value::Object(Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![Property {
            key: spa::sys::SPA_PARAM_BUFFERS_dataType,
            flags: PropertyFlags::empty(),
            value: Value::Choice(ChoiceValue::Int(Choice(ChoiceFlags::empty(), ChoiceEnum::Flags { default: data_type, flags: vec![data_type] }))),
        }],
    }));
    let meta = serialize(Value::Object(Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: ParamType::Meta.as_raw(),
        properties: vec![
            Property { key: spa::sys::SPA_PARAM_META_type, flags: PropertyFlags::empty(), value: Value::Id(Id(spa::sys::SPA_META_Header)) },
            Property {
                key: spa::sys::SPA_PARAM_META_size,
                flags: PropertyFlags::empty(),
                value: Value::Int(std::mem::size_of::<spa::sys::spa_meta_header>() as i32),
            },
        ],
    }));
    [buffers, meta]
}

/// The negotiated format's modifier: none (shared memory), fixed (DMA-BUF), or still open (the
/// compositor's list, for us to pick from).
enum Modifier {
    None,
    Fixed(u64),
    Open(Vec<u64>),
}

fn modifier_of(param: &Pod) -> Modifier {
    let Ok((_, Value::Object(object))) = spa::pod::deserialize::PodDeserializer::deserialize_any_from(param.as_bytes()) else {
        return Modifier::None;
    };
    let Some(p) = object.properties.iter().find(|p| p.key == FormatProperties::VideoModifier.as_raw()) else {
        return Modifier::None;
    };
    match &p.value {
        Value::Long(m) => Modifier::Fixed(*m as u64),
        Value::Choice(ChoiceValue::Long(Choice(_, ChoiceEnum::Enum { default, alternatives }))) if p.flags.contains(PropertyFlags::DONT_FIXATE) => {
            Modifier::Open(std::iter::once(*default).chain(alternatives.iter().copied()).map(|m| m as u64).collect())
        }
        Value::Choice(ChoiceValue::Long(Choice(_, ChoiceEnum::Enum { default, .. } | ChoiceEnum::None(default)))) => Modifier::Fixed(*default as u64),
        _ => Modifier::None,
    }
}

/// Runs the stream until `stop`: `on_frame` gets the newest frame of each wake-up (on this thread).
/// Returns when stopped, or with the reason the stream ended.
pub fn run(
    fd: Option<OwnedFd>,
    node: u32,
    fps: u32,
    modifiers: Vec<u64>,
    stop: Arc<AtomicBool>,
    mut on_frame: impl FnMut(Captured) + 'static,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    // The portal's remote; without one (the SHARKORD_TEST_NODE switch) the session's PipeWire
    let core = match fd {
        Some(fd) => context.connect_fd_rc(fd, None)?,
        None => context.connect_rc(None)?,
    };
    let stream = pw::stream::StreamRc::new(
        core,
        "sharkord-share",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    struct State {
        format: Option<Format>,
        failed: Option<String>,
        streaming: bool,
    }
    let state = std::rc::Rc::new(std::cell::RefCell::new(State { format: None, failed: None, streaming: false }));
    let quit = {
        let mainloop = mainloop.clone();
        move || mainloop.quit()
    };

    let listener = stream
        .add_local_listener_with_user_data(state.clone())
        .state_changed({
            let quit = quit.clone();
            move |_, state, _, new| {
                let mut s = state.borrow_mut();
                match new {
                    pw::stream::StreamState::Error(e) => {
                        s.failed = Some(format!("capture stream: {e}"));
                        quit();
                    }
                    pw::stream::StreamState::Streaming => s.streaming = true,
                    // The compositor ended the cast (the user stopped it, the window closed)
                    pw::stream::StreamState::Unconnected if s.streaming => {
                        s.failed = Some("the capture ended".into());
                        quit();
                    }
                    _ => {}
                }
            }
        })
        .param_changed(move |stream, state, id, param| {
            let Some(param) = param else { return };
            if id != ParamType::Format.as_raw() {
                return;
            }
            let mut info = spa::param::video::VideoInfoRaw::new();
            if info.parse(param).is_err() {
                return;
            }
            let (size, alpha) = ((info.size().width, info.size().height), info.format() == VideoFormat::BGRA);
            let m = modifier_of(param);
            eprintln!("capture: format {}x{} {:?}, modifier {}", size.0, size.1, info.format(), match &m {
                Modifier::None => "none (shared memory)".to_owned(),
                Modifier::Fixed(m) => format!("{m:#x}"),
                Modifier::Open(list) => format!("open, {} to pick from", list.len()),
            });
            let modifier = match m {
                // The compositor's modifier list, still open: fixate its first pick
                Modifier::Open(mods) => {
                    let fixed = format_pod(fps, None, Some(Format { size, alpha, modifier: mods.first().copied() }));
                    let mut params = [Pod::from_bytes(&fixed).expect("pod")];
                    let _ = stream.update_params(&mut params);
                    return;
                }
                Modifier::Fixed(m) => Some(m),
                Modifier::None => None,
            };
            let dmabuf = modifier.is_some();
            let f = Format { size, alpha, modifier };
            state.borrow_mut().format = Some(f);
            let pods = buffers_pods(dmabuf);
            let mut params: Vec<&Pod> = pods.iter().map(|p| Pod::from_bytes(p).expect("pod")).collect();
            let _ = stream.update_params(&mut params);
        })
        .process(move |stream, state| {
            // Only the newest buffer is used; the rest go back at once
            let mut newest: *mut pw::sys::pw_buffer = std::ptr::null_mut();
            loop {
                let b = unsafe { stream.dequeue_raw_buffer() };
                if b.is_null() {
                    break;
                }
                if !newest.is_null() {
                    unsafe { stream.queue_raw_buffer(newest) };
                }
                newest = b;
            }
            if newest.is_null() {
                return;
            }
            let release = Release { stream: stream.as_raw_ptr(), buffer: newest };
            let Some(format) = state.borrow().format else { return };
            let Some(data) = (unsafe { frame_data(newest) }) else { return };
            on_frame(Captured { format, data, release });
        })
        .register()?;

    // stderr goes to the app's log as [helper] lines
    eprintln!("capture: offering DMA-BUF with {} modifiers {:x?}, and shared memory", modifiers.len(), &modifiers[..modifiers.len().min(8)]);
    let formats = [format_pod(fps, Some(&modifiers), None), format_pod(fps, None, None)];
    let mut params: Vec<&Pod> = formats.iter().map(|p| Pod::from_bytes(p).expect("pod")).collect();
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    let timer = mainloop.loop_().add_timer({
        let quit = quit.clone();
        move |_| {
            if stop.load(Ordering::Relaxed) {
                quit();
            }
        }
    });
    timer.update_timer(Some(Duration::from_millis(100)), Some(Duration::from_millis(100))).into_result()?;
    mainloop.run();
    // The frame handler (and the converter in it) may still hold a buffer: it goes back before
    // the stream is disconnected, which frees the buffers
    drop(listener);
    let _ = stream.disconnect();
    match state.borrow_mut().failed.take() {
        Some(reason) => Err(anyhow!(reason)),
        None => Ok(()),
    }
}

/// Where the frame's pixels are, unless the compositor marked it corrupted or sent no picture.
unsafe fn frame_data(b: *mut pw::sys::pw_buffer) -> Option<Data> {
    unsafe {
        let buf = (*b).buffer;
        if buf.is_null() || (*buf).n_datas < 1 {
            return None;
        }
        let header = spa::sys::spa_buffer_find_meta_data(buf, spa::sys::SPA_META_Header, std::mem::size_of::<spa::sys::spa_meta_header>())
            as *const spa::sys::spa_meta_header;
        if !header.is_null() && (*header).flags & spa::sys::SPA_META_HEADER_FLAG_CORRUPTED != 0 {
            return None;
        }
        let d = &*(*buf).datas;
        let chunk = &*d.chunk;
        if chunk.flags & spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32 != 0 {
            return None;
        }
        match d.type_ {
            t if t == spa::sys::SPA_DATA_DmaBuf => Some(Data::Dmabuf { fd: d.fd as i32, size: d.maxsize as usize, offset: chunk.offset, stride: chunk.stride }),
            t if t == spa::sys::SPA_DATA_MemPtr || t == spa::sys::SPA_DATA_MemFd => {
                if d.data.is_null() || chunk.size == 0 {
                    return None;
                }
                Some(Data::Memory { ptr: (d.data as *const u8).add(chunk.offset as usize), len: chunk.size as usize, stride: chunk.stride })
            }
            _ => None,
        }
    }
}
