//! Native screen capture + hardware encode for the Electron client's screen share.
//!
//! Sharkord's page keeps its own WebRTC connection; the preload swaps each outgoing
//! placeholder frame's payload for a frame from here (see `electron/preload.js`). This
//! process only captures and encodes -- no network.
//!
//! Protocol, one share per process:
//! - stdin, JSON lines: `{"cmd":"start","codec":"h264"|"av1","width","height","fps","kbps",
//!   "label","primary","window"}` (on Windows label/primary pick the monitor, or `window` an app
//!   window by its HWND), `{"cmd":"keyframe"}`,
//!   `{"cmd":"bitrate","kbps"}`, `{"cmd":"stop"}`.
//!   EOF means the app is gone: stop and exit.
//! - stdout, records: a 16-byte header (u8 kind, u8 flags, u16 reserved, u32 LE length,
//!   u64 LE pts in microseconds) and the payload. Kind 1 is an encoded frame (flag 1 =
//!   keyframe), kind 2 a JSON event: on Linux `selected` / `cancelled` for the portal dialog,
//!   then `started` with the first frame (the sizes captured and encoded, the `api` and `device`,
//!   on Windows the `monitor`), `input` with what reaches the converter (format, size, and D3D11,
//!   DMA-BUF or shared memory, modifier), `stream` with the first H.264 keyframe's SPS profile and
//!   level, `stats` every 2 s (with the totals of pictures captured, encoded, repeated and
//!   skipped), `warning`, and `error` before the process gives up -- every failure after `start`
//!   sends one. `--check` prints `{"missing","h264","av1","api","device"}` (Linux: plus `driver`).
//!
//! Both encode with FFmpeg (`ffmpeg`), at the share's rate, keyframes only on request.
//!
//! Windows (`windows`): DXGI desktop duplication of the picked monitor, encoded by the GPU's own
//! encoder (AMD AMF, NVIDIA NVENC, Intel Quick Sync).
//!
//! Linux (Wayland, `linux`): a PipeWire stream the helper gets from the portal (`portal`),
//! converted and encoded on the GPU (Vulkan video, else VA-API), on bundled Mesa drivers where the
//! system's can't encode H.264. The helper shows the portal's dialog as soon as
//! it starts, before `start`, and reports `selected` (with the source's size in logical pixels) or
//! `cancelled`; Chromium doesn't capture at all, the page builds the share's stream (and its
//! preview) from these frames.

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

mod ffmpeg;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;
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
    /// An app window instead, by its HWND (Windows)
    #[cfg_attr(not(windows), allow(dead_code))]
    window: Option<u64>,
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
        window: v.get("window").and_then(|w| w.as_u64().or_else(|| w.as_str()?.parse().ok())),
    })
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

enum Command {
    Start(Value),
    Keyframe,
    Bitrate(u32),
    Stop,
}

fn main() -> Result<()> {
    #[cfg(target_os = "linux")]
    platform::prefer_bundled_driver();
    // `--check`: can this machine capture and encode, and which codecs? The packaging self-check
    // and the app's startup probe
    if std::env::args().any(|a| a == "--check") {
        let result = platform::check();
        println!("{result}");
        if result["missing"].as_array().is_some_and(|m| !m.is_empty()) {
            bail!("missing: {}", result["missing"]);
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
    let capture = match std::env::var("SHARKORD_TEST_NODE").ok().and_then(|n| n.parse().ok()) {
        Some(node) => platform::Capture { portal: None, node },
        None => match portal::select() {
            Ok(Some(p)) => {
                event(&out, json!({ "type": "selected", "source": p.size.map(|(w, h)| [w, h]) }));
                platform::Capture { node: p.node, portal: Some(p) }
            }
            Ok(None) => {
                event(&out, json!({ "type": "cancelled" }));
                return Ok(());
            }
            Err(e) => return fail(e),
        },
    };
    #[cfg(windows)]
    let capture: platform::Capture = ();

    let start = loop {
        match rx.recv() {
            Ok(Command::Start(v)) => break v,
            Ok(Command::Stop) | Err(_) => return Ok(()),
            Ok(_) => {}
        }
    };
    let share = match parse_start(&start).and_then(|s| platform::Share::start(&s, &out, capture)) {
        Ok(share) => share,
        Err(e) => return fail(e),
    };

    let share = Arc::new(share);
    let stop = Arc::new(AtomicBool::new(false));
    let pumping = {
        let (share, out, stop) = (share.clone(), out.clone(), stop.clone());
        std::thread::spawn(move || {
            if let Err(e) = share.run(&out, &stop) {
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
    share.finish();
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
