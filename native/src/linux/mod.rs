//! Linux: capture a PipeWire stream from the portal and encode it with FFmpeg (Vulkan, VA-API).

mod encode;

use std::sync::atomic::AtomicBool;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::{Out, Start};

/// What the capture needs to stay alive: the portal session.
pub type Capture = crate::portal::Portal;

pub fn check() -> Value {
    json!({ "missing": ["encoder"], "h264": false, "av1": false })
}

pub struct Share;

impl Share {
    pub fn start(_start: &Start, _out: &Out, _capture: Capture) -> Result<Share> {
        bail!("not implemented")
    }
    pub fn run(&self, _out: &Out, _stop: &AtomicBool) -> Result<()> {
        Ok(())
    }
    pub fn finish(&self) {}
    pub fn keyframe(&self) {}
    pub fn bitrate(&self, _kbps: u32) -> Result<()> {
        Ok(())
    }
}
