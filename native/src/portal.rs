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
/// For development only, `SHARKORD_PORTAL_TOKEN=<file>` keeps the portal's restore token there, so
/// repeated test runs skip the dialog after the first.
pub fn select() -> Result<Option<Portal>> {
    let token_file = std::env::var_os("SHARKORD_PORTAL_TOKEN");
    let token = token_file.as_ref().and_then(|f| std::fs::read_to_string(f).ok());
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
                    .set_persist_mode(if token_file.is_some() { PersistMode::ExplicitlyRevoked } else { PersistMode::DoNot })
                    .set_restore_token(token.as_deref().map(str::trim)),
            )
            .await?;
        let streams = match proxy.start(&session, None, Default::default()).await?.response() {
            Ok(streams) => streams,
            Err(ashpd::Error::Response(ResponseError::Cancelled)) => return Ok(None),
            Err(e) => return Err(e).context("screen selection"),
        };
        if let (Some(f), Some(t)) = (&token_file, streams.restore_token()) {
            let _ = std::fs::write(f, t);
        }
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
