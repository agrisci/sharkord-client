# AGENTS.md

Guide for AI agents working on the Sharkord desktop client. See [README.md](README.md) for
what the app does from a user's point of view — this file covers how the code is organized.

This is a thin Electron shell around a self-hosted [Sharkord](https://github.com/Sharkord/sharkord)
server: it loads the server's own web app and adds only what a browser can't do — a screen
share picker with share audio, hardware video encoding, and a way to pick/change the server.
There is no build step, no TypeScript and no bundler: plain CommonJS JavaScript and static HTML.

Core principle: **no over-engineering**. Follow the existing pattern, add the smallest thing
that works, and don't introduce abstractions or dependencies for a single use case. The whole
client is ~1300 lines on purpose.

## Architecture

| Path                         | What it is                                                                                   |
| ---------------------------- | -------------------------------------------------------------------------------------------- |
| `electron/main.js`           | Main process: settings, Chromium flags, venmic, screen picker, main window, server check, first launch / change server, open at login + tray, lifecycle |
| `electron/preload.js`        | Main window preload: `electronAPI` bridge, `getDisplayMedia` hook that adds share audio, `Notification` hook (taskbar flash; click shows the window and opens the channel/DM), injected "Change server" controls and desktop options in Settings → Others |
| `electron/picker.html`       | Screen share picker (source grid + audio step), styled like Sharkord                          |
| `electron/picker-preload.js` | `pickerAPI` bridge for the picker window                                                      |
| `electron/first-launch.html` | Server URL prompt (first run and Change server)                                               |
| `electron/unreachable.html`  | Shown when the server can't be reached or isn't a Sharkord server                             |
| `electron/theme.css`         | Sharkord's design tokens + card/input/button styles for the two pages above                  |
| `build/`                     | Icons packaged by electron-builder (committed so CI can build); `build/native/` is staged, not committed |
| `native/`                    | Native screen share helper (Rust + GStreamer): captures a monitor (Windows, DXGI) or a portal pick (Linux Wayland, PipeWire), encodes it on the GPU (AMF / VA-API), frames on stdout (see *Native screen share*) |
| `scripts/stage-native.js`    | Builds the helper and stages it into `build/native/` (shipped as `resources/native/`): on Windows with its GStreamer runtime subset, on Linux alone (it uses the system's GStreamer) |
| `.github/workflows/build.yml` | CI: builds on Linux + Windows runners for PRs into `dev`/`main` and pushes to `dev` (artifacts); run manually on `main` with a version bump to release |
| `upstream/`                  | Gitignored local clones of `sharkord` and `Vesktop`, for reference only — never edit or import |

`main.js` is split into sections with `// ── Name ──` banners; keep new code in the section it
belongs to rather than adding files.

## How the pieces talk

- **Settings**: `userData/settings.json` via `loadUserSettings` / `saveUserSettings`. Keys:
  `serverUrl` (saved without a trailing slash — use `savedServerUrl()`), `theme`
  (`'dark' | 'light'`, remembered from the page so local pages match it), `audio` (venmic
  options, merged over `AUDIO_DEFAULTS`), `minimizeToTray` (default off), `nativeShare` (default
  off; offered on Windows when the helper exists, on Linux Wayland when its `--check` probe at
  startup finds the elements and a VA-API encoder). *Open at login* is not
  stored: the OS login item / `~/.config/autostart/sharkord.desktop` is the source of truth.
  Always spread the existing settings when saving.
- **IPC** (`ipcMain.handle` / `ipcRenderer.invoke` unless noted):
  - Page → main: `virtmic-active`, `virtmic-unmute`, `virtmic-stop`, `change-server` (`send`),
    `desktop-settings-get`, `desktop-settings-set`, `notification-shown` / `notification-clicked`
    (`send`), `native-share-pick` (Linux), `native-share-target`, `native-share-start` /
    `native-share-stop` (`send`) — only accepted from the main window's webContents.
  - Main → page: `native-share-port` (a `MessagePort` tagged with the share's `id`, forwarded
    into the page world with `window.postMessage`): helper frames and events one way,
    `keyframe`/`bitrate`/`stop` the other.
  - Picker → main: `virtmic-list`, `audio-settings-get`, `audio-settings-set`,
    `picker-go-live`, `picker-cancelled` (`send`).
  - Main → picker: `init` (sources, `skipPicker`, `platform`, `theme`).
- **First-launch window** has no preload. It reports back through `console-message`:
  `cancel` or `form:{json}` (same channel Vesktop's first-launch view uses). Main answers
  with `executeJavaScript('setError(...)')`.
- **Unreachable page** buttons are links to `sharkord://retry` and `sharkord://change-server`,
  intercepted by the main window's `will-navigate`.
- **Local pages** get their state through `loadFile(..., { query })` (`theme`, `url`,
  `current`, `reason`) and apply the theme class before first paint.
- **Tray / no menu bar**: there is no application menu (`Menu.setApplicationMenu(null)`);
  shortcuts are handled in `before-input-event`. The tray menu (Open, Change Server…, Open at
  login, Minimize to tray, Quit) is the non-DOM fallback. With *Minimize to tray* on, `close`
  hides the window unless `quitting`. Autostart launches with `--hidden` (starts in the tray,
  only if *Minimize to tray* is on). A single-instance lock makes a second launch show the window.
- **Server check**: `checkServer` fetches `<url>/info` and expects `serverId` and `name`
  strings → `'ok' | 'not-sharkord' | 'unreachable'`. `loadServer` checks before loading —
  navigating to an unreachable URL and then to a local page can leave the window unable to paint.

## Screen share flow

1. Sharkord calls `getDisplayMedia` → `handleDisplayMediaRequest` (set with
   `session.setDisplayMediaRequestHandler`). Any previous venmic link is dropped first.
2. `desktopCapturer.getSources`. On **Wayland** this shows the system portal and returns only
   its pick, so the picker opens straight on the audio step (`skipPicker`). An empty list means
   the portal was cancelled.
3. The picker calls `picker-go-live`:
   - **Linux**: venmic links the chosen apps (or `Entire System`) into the virtual mic
     `vencord-screen-share`. `buildLinkData` always excludes Chromium's `Audio Service` PID so
     the user's own call audio never gets shared.
   - **Windows**: `streams.audio = 'loopback'` (or `'loopbackWithoutChrome'` on 10 22H2+).
4. `finishPick(streams)` answers the request. `null` denies it — Electron 44 throws on `callback({})`.
5. The preload's `getDisplayMedia` wrapper (installed with `contextBridge.executeInMainWorld`
   so it runs in the page's world) swaps the virtual mic's track into the stream, hides the
   virtual mic from `enumerateDevices`, and calls `virtmic-stop` when the share ends. Sharkord
   ends shares with `track.stop()`, which fires no `ended` event, so both are hooked.

### Native screen share (Windows, Linux Wayland; experimental)

With the `nativeShare` setting on (or `SHARKORD_NATIVE_SHARE=1`), a share's video is captured and
encoded outside Chromium by the helper in `native/`, while Sharkord and Chromium keep everything
else (connection, packetization, bandwidth estimate).

- **Windows** (whole screens, AMD): `d3d11screencapturesrc` → `d3d11convert` → AMF H.264/AV1.
  Chromium's capture still runs for the local preview and share audio.
- **Linux** (Wayland, VA-API): the helper owns the pick, so the portal asks once. The page's
  `getDisplayMedia` hook calls `native-share-pick` instead of Chromium's; main spawns the helper,
  which opens its own ScreenCast portal session at once (`native/src/portal.rs`, `ashpd`; monitors
  and windows, nothing persisted) and answers `selected` or `cancelled`, then main shows the
  picker's audio step. The page builds Sharkord's stream itself: the video is a
  `MediaStreamTrackGenerator` fed with the helper's frames decoded by `VideoDecoder` (the local
  preview), the audio venmic's virtual mic (`withShareAudio`). The graph: `pipewiresrc` (no
  clock, buffers re-stamped on arrival, `keepalive-time` at the frame period) → `vapostproc`
  copy into VA memory at once (the compositor lends only a few buffers) → the same
  videorate/queue → `vapostproc` → `vah264enc`/`vaav1enc` (VBR at 100% of the target: CBR pads a
  still screen; AV1 without reordering). These were measured on KWin (Renoir
  and RX 9060 XT), and took a Renoir iGPU from ~41 to 60 fps. Before the pick the page reads
  the screen codec from Sharkord's Devices settings (`sharkord-devices-settings` in its
  localStorage, `screenCodec`): the helper encodes only H.264/AV1, so VP8, VP9 and `auto` stay
  entirely Chromium's (its capture, its picker). This is the one place the client reads Sharkord's
  state; an unreadable value counts as native. With no Chromium capture in a native share there is
  nothing to fall back to, so every fallback below **ends** it (the `ended` event is sent again
  every second until Sharkord stops the track: at the very start Sharkord isn't listening yet);
  later shares use Chromium's path. Tried and dropped: reading Chromium's own PipeWire stream (one portal pick too) -- Chromium
  fixes a tiled DMA-BUF modifier VA can't import, and the GL read-back pinned a CPU core.

1. Windows: `picker-go-live` records the picked monitor (`_nativeTarget`); windows stay on
   Chromium. Linux: `native-share-pick` sets it once the helper has a pick.
2. The page hook (`installNativeShare`) returns Sharkord the real stream but substitutes a
   placeholder track (`MediaStreamTrackGenerator`, 320x180 black) wherever it is handed to a
   connection (`addTransceiver`/`addTrack`/`replaceTrack`), and attaches an encoded transform.
   Placeholder frames are built in memory (I420), not on a canvas: a canvas frame is read back from
   the GPU for the software encoder and stalled at 15 fps while the GPU was busy. The track has no
   `contentHint`: as screen content Chromium's periodic probes kept knocking the estimate down.
3. The worker learns the negotiated codec from the first encoded frame; for H.264/AV1 the page
   calls `native-share-start` and main spawns the helper and hands the page a `MessagePort`.
   Anything else is handled by `s.fallback`: VP8/VP9, simulcast, a helper `error` event (the
   helper sends one for every failure after `start`, e.g. no AMD GPU), the helper exiting on its
   own (main reports it as an `error`), and the watchdog (no helper frame for 10 s, or none swapped
   for 6 s). Before any native frame went out, the connection just gets the real track. Mid-share
   the share **ends** (`ended` on the real track, which Sharkord handles like a stopped capture)
   and later shares in the session use Chromium's own path: continuing in place froze viewers
   (swapping the track in restarts the RTP timestamps from the capture's older clock) or stalled
   (the hardware encoder Chromium switches to mid-share at <= 1080p). A share must never be left
   on the black placeholder.
   For H.264 the helper pins `profile=constrained-baseline` in caps (what Sharkord's `42e01f`
   promises; AMF takes the profile from downstream caps) and reports the first keyframe's SPS as
   a `stream` event, so the log shows what viewers really get.
4. Each helper frame gets one placeholder frame (paced: Chromium thins out bursts), and the worker
   swaps its payload into the matching outgoing frame. Chromium derives the RTP timestamp from the
   placeholder's, so frames pair by timestamp and size. Keyframes must line up (receivers and
   mediasoup trust Chromium's frame type): a native keyframe changes the placeholder's size,
   which makes Chromium key that exact frame (Chromium 152 has no `generateKeyFrame()` here).
5. The helper's bitrate follows Chromium's transport estimate (`availableOutgoingBitrate`), not
   `targetBitrate`: Chromium counts the bytes the transform adds as post-encode overhead and halves
   the encoder target, and its estimate only grows to 1.5x what is acknowledged. While loss, round
   trip and Chromium's pacer queue stay clean it only steps up with the estimate (which dips 15-50%
   every few seconds on a clean LAN with mediasoup); on congestion (loss, a growing round trip, or
   packets waiting over 50 ms in the pacer, i.e. the estimate really below what goes out) it drops
   to 0.85x. Capped by Sharkord's bitrate
   setting and ~25 Mbps at 4K60; the resolution stays what the user picked.
   `getStats` is rewritten so Sharkord's stats show the helper's encoder and size. Main kills the
   helper on every exit path (`stopNativeShare`; the page sends `native-share-stop` for a share
   that ends before `native-share-start`, as the Linux helper already holds the portal session).

## Rules

- **Don't modify Sharkord.** The client loads the real web app unchanged. Anything added to its
  UI goes through `preload.js`, anchored on a `data-testid` that Sharkord's own e2e tests use,
  cloned from that element so it keeps Sharkord's styling, and marked with
  `data-client-change-server`-style attributes to avoid duplicates. If the anchor disappears the
  control is simply not added — never throw or break the page. Keep a non-DOM fallback (tray
  menu item / shortcut) for anything injected.
- Every window keeps `contextIsolation: true` and `nodeIntegration: false`. Expose new
  capabilities as small named functions on the existing bridges, not raw `ipcRenderer`.
- Only **one** `enable-features` switch may be set (a second call overrides the first) — add
  to the existing list in `main.js`.
- Local pages look like Sharkord: reuse `theme.css` classes, and when you need a new style or
  icon, copy it from `upstream/sharkord` (tokens, component classes, lucide icons) instead of
  inventing one. Support both `dark` and `light`.
- Linux-only / Windows-only code is guarded with `process.platform` checks; venmic must stay
  optional (`obtainVenmic()` returns `null` when it can't load).
- Clean up on every exit path: close the picker, `venmicUnlink()` and deny pending
  display-media callbacks when windows close or the server changes.

### Code style

- 2-space indent, no semicolons, single quotes, dense aligned one-liners like the surrounding
  code. `picker.html`'s inline script is ES5 (`var`, `function`) — match it there.
- Short comments that explain *why* (platform quirks, Electron/Chromium behaviour), not what.
- Log through `log()` with a `[tag]` prefix (`[venmic]`, `[load]`, `[check]`, `[gpu]`).
- New dependencies need a real reason; the only runtime dependency is `@vencord/venmic`
  (optional, Linux-only; listed in `asarUnpack` because it's a native module).

## Commands

```sh
npm install
npm start               # run the app (DevTools: Ctrl+Shift+I)
npm run dist:linux      # AppImage + deb + rpm → release/
npm run dist:win        # NSIS installer       → release/ (stages the native helper first)
npm run dist:all
npm run stage:native    # build + stage native/ only
```

The native helper needs Rust (`native/rust-toolchain.toml`) and, on Windows, GStreamer 1.28 MSVC
with its development files (`/TYPE=devel`), plus `pkg-config`; on Linux the distro's GStreamer
development packages (Fedora `gstreamer1-devel gstreamer1-plugins-base-devel`, Debian/Ubuntu
`libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev`). At runtime Linux needs GStreamer 1.22+
with the `va` plugin (plugins-bad) and `pipewiresrc`, and a VA driver that encodes: Fedora's own
Mesa has H.264 encoding compiled out (RPM Fusion's `mesa-va-drivers-freeworld` has it). Without
the build tools `dist:*` still builds, only without the native share; CI sets
`SHARKORD_REQUIRE_NATIVE=1` to fail instead. For `npm start` just `cd native && cargo build
--release` (on Windows with `PKG_CONFIG_PATH` pointing at GStreamer's `lib\pkgconfig`).

Versioning is SemVer, independent of Sharkord's server version, and stays `0.x` while Sharkord
is alpha (minor = features, patch = fixes). Don't bump `version` by hand in feature branches.
Releases: merge `dev` into `main`, then Actions → Build → Run workflow on `main` with a bump —
CI builds, commits `Release vX.Y.Z` to `main`, tags it, publishes the Release (changelog from
commit subjects, so keep them readable) and merges the release commit back into `dev`.
`@vencord/venmic` is an `optionalDependency` because it's Linux-only (`"os": ["linux"]`);
`npm ci` on Windows skips it.

Hardware encoding tip for testing screen share: pick **H264** and turn **Simulcast off** in
Sharkord's Devices settings (simulcast forces VP8, which most GPUs can't encode). Check
`chrome://gpu` or the `[gpu]` log lines for VA-API status. To confirm hardware encoding, look at
the sender's `outbound-rtp` `encoderImplementation` (e.g. `MediaFoundationVideoEncodeAccelerator`
on Windows, not `OpenH264`). On Windows it relies on `PlatformH264CbpEncoding`: without it
Chromium encodes Constrained Baseline H.264 (`42e01f`, what Sharkord negotiates) in software,
and on `WebRtcAllowWgcUsingTexture`: without it 4K screen capture stalls around 36 fps (compare
the `media-source` `framesPerSecond` with the encoder's). AV1 uses `WebRtcAV1HWEncode` with
`ExpandMediaFoundationEncodingResolutions` disabled, because Chromium drops AMD's AV1 encoder when
it can't query its resolutions. That feature is on by default in Chromium; turning it off caps
every hardware encoder, on every GPU, at 1080p, so 1440p/4K shares on Chromium's path fall back to
software (`OpenH264` / `libaom`). NVIDIA never gets hardware Constrained Baseline H.264 in Chromium
(crbug 1088650), and texture capture fails for good if the GPU adapter changes (hybrid laptops).
All four flags are experimental: see ROADMAP.md #88-#91 before relying on or changing them.

## Manual test checklist

There are no automated tests. After a change, check what it touches:

- **First launch** (delete `serverUrl` from `settings.json`): invalid URL, a non-Sharkord
  site, an unreachable host (error shown, nothing saved), a valid server, Cancel (quits).
- **Change server**: ☰ server menu item, login-screen button, `Ctrl+Shift+O`, tray →
  **Change Server…**; Cancel returns to the current server.
- **Desktop settings**: top of Settings → **Others** (user settings only, not server settings).
  *Open at login* creates/removes the autostart entry; *Minimize
  to tray* makes X hide the window, tray click reopens it, tray **Quit** exits. Toggles stay in
  sync with the tray menu. Launching again focuses the running window.
- **Notifications**: enable them in Sharkord → Settings → Notifications; a message from another
  account while the channel isn't open (or the window is hidden) shows a native notification,
  flashes the taskbar until focused (X11; Wayland ignores it), and clicking it brings the window
  back (also from the tray) and opens that channel or DM — test from the channel list and from the
  DM list. On Windows (installed build) the toast is labelled Sharkord.
- **Unreachable page**: stop the server → Retry and Change server both work.
- **Theme**: switch Sharkord to light, restart with the server down — local pages and the
  picker should be light too.
- **Screen share picker**: on X11 (source grid) and Wayland (portal, then audio step); cancel
  at each step denies the share cleanly.
- **Share audio (Linux)**: per-app and Entire System; the other side must not hear their own
  voice echoed; audio unlinks when the share stops or the picker is closed.
- **Share audio (Windows)**: "Stream With Audio" loopback.
- **Native screen share (Windows, AMD)**: turn it on in Settings → Others (or the tray). Share a
  screen with H.264, then AV1, simulcast off: a viewer gets 60 fps, rejoining shows a picture
  within a second, and Sharkord's stats show `sharkord-share (amf…)`. The console's
  `[native-share] sent …` lines should keep `lost`/`resync` near zero. A window share, VP8, or
  the setting off must behave exactly as before.
- **Native screen share (Linux Wayland, VA-API)**: the switch appears only when the startup probe
  passes (`[native-share] probe` in the log). Share with H.264, simulcast off: **one** portal
  dialog then the audio step, the local preview moves, a viewer gets 60 fps, stats show
  `sharkord-share (vah264enc)`. Cancelling the portal dialog or the audio step cancels the share
  and the desktop's sharing indicator goes away; stopping the share ends the helper. VP8 in
  Sharkord's Devices settings: Chromium's picker, not the helper's, and a moving preview.

## Commits and privacy

This repo is public: `agrisci/sharkord-client`.

- **No AI attribution** — no `Co-Authored-By: Claude` or similar trailers in commits or PRs.
- Never rewrite published history (including the `Initial commit`): no force-pushes to `dev` or `main`.
- Keep personal info out of code, docs and commits: no personal emails, no real server URLs
  (use `https://sharkord.example.com`).
- Branches: work on a feature branch off `dev` and open the PR against `dev` (not `main`);
  don't commit straight to `dev`. CI must pass on the PR.
- Commit messages: short imperative subject, blank line, `-` bullet body describing each change.
