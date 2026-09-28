//! Linux (Wayland): the screen comes from xdg-desktop-portal's ScreenCast, as a PipeWire stream.
//!
//! This is the share's only portal dialog: Chromium doesn't capture at all (the page builds the
//! share's stream from this helper's frames). Nothing is persisted (no restore token): every share
//! asks again, so the user can pick another screen each time.

use std::os::fd::OwnedFd;

use anyhow::{Context, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, ResponseError, Session};
use ashpd::enumflags2::BitFlags;

/// A started screen cast. The proxy holds the D-Bus connection: the portal ends the session
/// (and the stream) when it closes, so this must live as long as the pipeline.
pub struct Portal {
    pub fd: OwnedFd,
    pub node: u32,
    pub size: Option<(u32, u32)>,
    _session: Session<Screencast>,
    _proxy: Screencast,
}

/// Shows the portal's dialog and opens the picked screen or window. `Ok(None)`: the user cancelled.
pub fn select() -> Result<Option<Portal>> {
    ashpd::zbus::block_on(async {
        let proxy = Screencast::new().await.context("no screen cast portal")?;
        let session = proxy.create_session(Default::default()).await?;
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::Embedded)
                    .set_sources(BitFlags::from(SourceType::Monitor) | SourceType::Window)
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await?;
        let streams = match proxy.start(&session, None, Default::default()).await?.response() {
            Ok(streams) => streams,
            Err(ashpd::Error::Response(ResponseError::Cancelled)) => return Ok(None),
            Err(e) => return Err(e).context("screen selection"),
        };
        let stream = streams.streams().first().context("the portal returned no stream")?;
        let (node, size) = (stream.pipe_wire_node_id(), stream.size());
        let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;
        Ok(Some(Portal {
            fd,
            node,
            size: size.and_then(|(w, h)| Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?))),
            _session: session,
            _proxy: proxy,
        }))
    })
}
