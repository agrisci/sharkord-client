# AGENTS.md

Guide for AI agents working on the Sharkord desktop client. See [README.md](README.md) for
what the app does from a user's point of view — this file covers how the code is organized.

This is a thin Electron shell around a self-hosted [Sharkord](https://github.com/Sharkord/sharkord)
server: it loads the server's own web app and adds only what a browser can't do — a screen
share picker with share audio, hardware video encoding, and a way to pick/change the server.
There is no build step, no TypeScript and no bundler: plain CommonJS JavaScript and static HTML.

Core principle: **no over-engineering**. Follow the existing pattern, add the smallest thing
that works, and don't introduce abstractions or dependencies for a single use case. The Electron
side is ~2300 lines and the native helper ~2300 (`native/`, `scripts/stage-native.js`, `scripts/deps/`), on purpose.

## Architecture

| Path                         | What it is                                                                                   |
| ---------------------------- | -------------------------------------------------------------------------------------------- |
| `electron/main.js`           | Main process: settings, Chromium flags, venmic, screen picker, native share helper (probe, spawn, frames), main window, server check, first launch / change server, open at login + tray, lifecycle |
| `electron/preload.js`        | Main window preload: `electronAPI` bridge, `getDisplayMedia` hooks (share audio; the native share's placeholder swap, frame worker and decoded preview), `Notification` hook (taskbar flash; click shows the window and opens the channel/DM), injected "Change server" controls and a **Desktop Client** tab in the user settings |
| `electron/picker.html`       | Screen share picker (source grid + audio step), styled like Sharkord                          |
| `electron/picker-preload.js` | `pickerAPI` bridge for the picker window                                                      |
| `electron/first-launch.html` | Server URL prompt (first run and Change server)                                               |
| `electron/unreachable.html`  | Shown when the server can't be reached or isn't a Sharkord server                             |
| `electron/theme.css`         | Sharkord's design tokens + card/input/button styles for the two pages above                  |
| `build/`                     | Icons packaged by electron-builder (committed so CI can build); `build/native/` is staged, not committed |
| `native/`                    | Native screen share helper (Rust): captures a monitor (Windows: GStreamer, DXGI → AMF, `src/gst.rs`) or a portal pick (Linux Wayland: PipeWire → FFmpeg's Vulkan video or VA-API encoder, `src/linux/`), frames on stdout (see *Native screen share*) |
| `scripts/stage-native.js`    | Builds the helper and stages it into `build/native/` (shipped as `resources/native/`): on Windows with its GStreamer runtime subset, on Linux with the bundled Mesa Vulkan drivers (`mesa/`) and licences |
| `scripts/deps/`              | Linux: `ffmpeg.sh` (static LGPL FFmpeg with our runtime-bitrate patches), `mesa.sh` (RADV/ANV with H.264 encode, the helper's fallback), `ubuntu-packages.sh` (their build dependencies, CI), `container.sh` (the same in Ubuntu 22.04 via podman) |
| `.github/workflows/build.yml` | CI: builds on Linux + Windows runners for PRs into `dev`/`main` and pushes to `dev` (artifacts); run manually on `main` with a version bump to release |
| `upstream/`                  | Gitignored local clones for reference only (`sharkord-src`, `Sunshine`, ...) — never edit or import |

`main.js` is split into sections with `// ── Name ──` banners; keep new code in the section it
belongs to rather than adding files.

## How the pieces talk

- **Settings**: `userData/settings.json` (`~/.config/sharkord`, `%APPDATA%\sharkord`) via `loadUserSettings` / `saveUserSettings`. Keys:
  `serverUrl` (saved without a trailing slash — use `savedServerUrl()`), `theme`
  (`'dark' | 'light'`, remembered from the page so local pages match it), `audio` (venmic
  options, merged over `AUDIO_DEFAULTS`), `minimizeToTray` (default off), `startMinimized` (a login launch stays in the
  tray), `nativeShare` (default
  off; usable only when the helper's `--check` probe at startup encodes with a codec -- AMF on
  Windows, Vulkan video or VA-API on Linux Wayland -- and a share goes native only with a codec it found; where it
  can't run, its switch is greyed out and `nativeShareNote` says why, and `nativeShareCodecs` lists
  what the probe found the GPU hardware encodes, H.264 / AV1 with a check or a cross),
  `chromiumHwEncode` (hardware encoding for shares on Chromium's own path, i.e. the Chromium flags
  below; default on on Windows, off on Linux; read once at launch, `SHARKORD_CHROMIUM_DEFAULTS=1`
  forces it off). *Open at login* is not
  stored: the OS login item / `~/.config/autostart/sharkord.desktop` is the source of truth.
  Always spread the existing settings when saving.
- **IPC** (`ipcMain.handle` / `ipcRenderer.invoke` unless noted):
  - Page → main: `virtmic-active`, `virtmic-unmute`, `virtmic-stop`, `change-server` (`send`),
    `desktop-settings-get`, `desktop-settings-set`, `notification-shown` / `notification-clicked`
    (`send`), `native-share-pick` (Linux) and `native-share-target` (both with the share's codec),
    `native-share-start` / `native-share-stop` (`send`) — only accepted from the main window's
    webContents.
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
  login, Start minimized, Minimize to tray, Quit) is the non-DOM fallback. With *Minimize to tray*
  on, `close` hides the window unless `quitting`. Autostart launches with `--hidden`, which only
  marks a login launch: it starts in the tray when *Start minimized* is on (independent of
  *Minimize to tray*, like Discord's and Vesktop's). A single-instance lock makes a second launch show the window.
- **Server check**: `checkServer` fetches `<url>/info` and expects `serverId` and `name`
  strings → `'ok' | 'not-sharkord' | 'unreachable'`. `loadServer` checks before loading —
  navigating to an unreachable URL and then to a local page can leave the window unable to paint.

## Screen share flow

Chromium's own share, used whenever the native share isn't (see below; on Linux a native share
replaces steps 1-3 with the helper's portal pick and the picker's audio step).

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

With the `nativeShare` setting on (or `SHARKORD_NATIVE_SHARE=1`), some shares are captured and
encoded outside Chromium by the helper in `native/`, while Sharkord and Chromium keep everything
else (connection, packetization, bandwidth estimate). **Chromium's own share is the default**; the
helper is opt-in per share, so nothing it can't handle ever reaches it.

**Which path a share takes.** At startup `probeNativeShare` runs the helper's `--check` once: it
opens each codec's encoder (AMF on Windows; on Linux it encodes a frame with Vulkan video, else
VA-API, on the system's drivers or the bundled ones, reported as `api`/`driver`) and reports
`h264`/`av1`. No codec →
the switch is greyed out, with the reason under it. When a share starts, the page reads
`screenCodec` and `simulcastEnabled` from Sharkord's Devices settings (`sharkord-devices-settings`
in its localStorage), and whether the server allows simulcast: `webRtcSimulcastEnabled`, one of
the server's public settings, read from Sharkord's own WebSocket messages as they arrive (never
changed; until seen, the user's switch alone decides). These are the only places the client reads
Sharkord's state; a renamed key or field only turns the helper off:

| Sharkord's Devices settings | Encoder opened at startup | Share |
|---|---|---|
| H.264 or AV1, no simulcast (switch off, or not allowed by the server) | yes | **helper** |
| H.264 or AV1, no simulcast | no (e.g. AV1 on a GPU without AV1 encoding) | Chromium |
| Simulcast on, and allowed by the server | -- (Sharkord shares VP8) | Chromium |
| VP8, VP9 | -- (the helper never encodes them) | Chromium |
| `auto`, missing, unreadable | -- (codec not known in advance) | Chromium |

"Chromium" is exactly the share with the option off: Chromium's picker, capture and encoders.

**How a helper share runs.**

| | Windows (AMD, whole screens) | Linux (Wayland; AMD, Intel) |
|---|---|---|
| The rule is applied | after the pick (`native-share-target`, with the codec) | before the pick (`native-share-pick`, with the codec) |
| Pickers | our source grid, then the audio step | the helper's portal dialog (once), then the audio step |
| Capture | helper **and** Chromium's own, still running | helper only |
| Local preview | Chromium's capture | the helper's frames, decoded in the page |
| Graph | `d3d11screencapturesrc` → `d3d11convert` → AMF | PipeWire (DMA-BUF) → `scale_vulkan` → `h264_vulkan` / `av1_vulkan` (below) |
| Windows (app windows) | Chromium's share | helper (the portal offers them) |

**When it fails anyway** (the `s.fallback` cases; a share is never left on the black placeholder):

| Failure | Windows | Linux |
|---|---|---|
| Before the first native frame (the encoder won't start despite the probe, a codec other than the settings said) | the connection gets Chromium's capture; the share goes on | the share **ends** (nothing to fall back to); later shares that session are Chromium's |
| Mid-share (helper error or exit, watchdog: no frame for 10 s / none swapped for 6 s) | the share ends; later shares that session are Chromium's (logged at each share), unless the system was suspended during it (`powerMonitor`, sent as a `suspend` event on the share's port) | same |
| A picker or the audio step cancelled | no share | no share; the helper is stopped |

A share ends through `ended` on Sharkord's track, sent again every second until Sharkord stops it
(at the very start Sharkord isn't listening yet).

**Linux details.** The page's `getDisplayMedia` hook calls `native-share-pick` instead of
Chromium's; main spawns the helper, which opens its own ScreenCast portal session at once
(`native/src/portal.rs`, `ashpd`; monitors and windows, nothing persisted) and answers `selected`
or `cancelled`, then main shows the picker's audio step. The page builds Sharkord's stream itself:
a `MediaStreamTrackGenerator` fed with the helper's frames decoded by `VideoDecoder`, and venmic's
virtual mic (`withShareAudio`).

The helper (`native/src/linux/`) has no GStreamer: `capture.rs` reads the portal's PipeWire stream
and `encode.rs` converts and encodes with a static FFmpeg of our own (`scripts/deps/ffmpeg.sh`):
- **Capture.** DMA-BUF with the modifiers the GPU can import (asked through Vulkan; KWin then picks
  a tiled one), shared memory when the compositor offers nothing else. Only the newest buffer is
  kept, the rest go back at once, and that one as soon as the GPU has read it (`Device::wait` on the
  converted picture): KWin lends 3 buffers and stops sending while they are held, and FFmpeg's own
  release came later -- capture stalled after 3 frames. 4K60 zero-copy measured at 6-9% CPU. The size comes from the negotiated format, i.e. physical pixels (the
  portal's is logical, #99).
- **Conversion** on the GPU: `hwmap` of the DMA-BUF into Vulkan → `scale_vulkan` to NV12 at the
  fitted size (`fit`), no copy through memory.
- **Encoder**: `h264_vulkan`/`av1_vulkan` (VA-API's `h264_vaapi`/`av1_vaapi` if Vulkan can't),
  an **8-frame VBV** with VBR on Vulkan (a still screen costs ~130 kbps; with video the rate stays
  within 6% of the target) and CBR on VA-API (its VBR ignored the VBV), and no frame larger than that buffer (Vulkan's `maxFrameSize`, which
  FFmpeg leaves off: RADV let scene cuts in a video burst to 740-921 KB, now at most ~190 KB),
  keyframes only on request (no GOP), no B-frames, H.264 Constrained Baseline with CAVLC, BT.709. Measured offline at 4K60 21 Mbps (still desktop, scrolling text, Big Buck
  Bunny, RX 9060 XT): VMAF 95.9 on video against 91.8 for GStreamer's `vah264enc` CBR, the rate on
  target, keyframes ~370-515 KB. `vah264enc` couldn't cap frame size (`cpb-size` is inert): a still
  4K screen keyed at 1.1-1.8 MB, more than Chromium's hardware decoder path absorbs (the viewer
  asked for keyframes 4x/s, each as large: a storm at 0-7 fps on a clean link), and its VBR sent
  28-39 Mbps asked for 21. A one-frame VBV keyed at 60-96 KB but stayed blurry for ~0.9 s.
- **Pacing**: an encoder thread ticks at the share's rate and repeats the last picture while the
  screen is still (the compositor only sends on damage); pts come from that tick, so the
  compositor's clock never meets ours, and a busy encoder skips a tick instead of queueing. This
  replaced GStreamer's `videorate`, leaky queue, `keepalive-time` and clock-less pipeline.
- **Bitrate** changes apply to the next frame without a keyframe (FFmpeg otherwise sends rate
  control only with the first picture on Vulkan, and only with IDRs on VA-API; the drivers accept it
  on any frame). Our FFmpeg patches (`scripts/deps/ffmpeg-*.patch`) do that and the frame cap.
- **Drivers**: the system's Mesa first. Fedora and openSUSE build Mesa without H.264 (VA-API and
  Vulkan), so the package ships RADV and ANV built with it (`scripts/deps/mesa.sh`, `resources/
  native/mesa/`): when the system's can't encode H.264, the helper re-runs itself with the Vulkan
  loader pointed at them (`VK_DRIVER_FILES`, `SHARKORD_BUNDLED_DRIVER=1`). Only the helper loads
  them. NVIDIA needs NVIDIA's own driver (nouveau can't encode).
- For testing: `SHARKORD_TEST_NODE=<PipeWire node id>` captures that node without the portal's
  dialog, `SHARKORD_PORTAL_TOKEN=<file>` keeps the portal's restore token there (one dialog, then
  none), `SHARKORD_ENCODE_API=vulkan|vaapi` forces an API. The helper's stderr (`capture:` lines:
  the modifiers offered and the format agreed) goes to the app's log as `[helper]`. `cargo test -- --ignored` runs a GPU
  test (4K, keyframes on request only and ≤ 600 KB, a bitrate change applied without one).

Tried and dropped: reading Chromium's own PipeWire stream (one portal pick too) -- Chromium fixes a
tiled DMA-BUF modifier VA couldn't import, and the GL read-back pinned a CPU core.

**Step by step:**

1. Windows: `picker-go-live` records the picked monitor (`_nativeTarget`); windows stay on
   Chromium. Linux: `native-share-pick` sets it once the helper has a pick.
2. The page hook (`installNativeShare`) returns Sharkord its stream (Windows: Chromium's capture;
   Linux: the one it built) but substitutes a
   placeholder track (`MediaStreamTrackGenerator`, 320x180 black) wherever it is handed to a
   connection (`addTransceiver`/`addTrack`/`replaceTrack`), and attaches an encoded transform.
   Placeholder frames are built in memory (I420), not on a canvas: a canvas frame is read back from
   the GPU for the software encoder and stalled at 15 fps while the GPU was busy. The track has no
   `contentHint`: as screen content Chromium's periodic probes kept knocking the estimate down.
3. The worker learns the negotiated codec from the first encoded frame; for H.264/AV1 the page
   calls `native-share-start` and main spawns the helper (Linux: reuses the one from the pick) and
   hands the page a `MessagePort`. Anything unexpected goes through `s.fallback` (the failure
   table above): a codec other than the one read from the settings, simulcast anyway, a helper `error`
   event (the helper sends one for every failure after `start`), the helper exiting on its own
   (main reports it as an `error`), and the watchdog. Continuing a failed share in place didn't
   work: swapping a capture track in restarts the RTP timestamps from its older clock (viewers
   dropped every frame as stale), and the hardware encoder Chromium switches to mid-share at
   <= 1080p stalled.
   For H.264 the helper pins Constrained Baseline (what Sharkord's `42e01f` promises: in caps on
   Windows, where AMF takes the profile from downstream; FFmpeg's `profile` on Linux) and reports the first keyframe's SPS as
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
   packets waiting over 250 ms in the pacer, i.e. the estimate really below what goes out; a
   keyframe's 50-180 ms doesn't count) it drops to 0.85x.
   AMF restarts with a keyframe on every bitrate change, so there it steps 30% at least 4 s apart;
   the Linux helper changes it in place, so 10% at least 2 s apart. Capped by Sharkord's bitrate
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
- Chromium features go in the lists in `main.js`'s Chromium flags section, which merges them with
  the command line's and appends one `enable-features` / `disable-features` each (a second
  `appendSwitch` would override the first); never call `appendSwitch` for features elsewhere. The
  `[flags]` log lines show what was applied. Don't add GPU blocklist overrides
  (`ignore-gpu-blocklist`, `enable-gpu-rasterization`, ...): the blocklist covers broken drivers.
- Local pages look like Sharkord: reuse `theme.css` classes, and when you need a new style or
  icon, copy it from `upstream/sharkord` (tokens, component classes, lucide icons) instead of
  inventing one. Support both `dark` and `light`.
- Linux-only / Windows-only code is guarded with `process.platform` checks; venmic must stay
  optional (`obtainVenmic()` returns `null` when it can't load).
- Clean up on every exit path: close the picker, `venmicUnlink()` and deny pending
  display-media callbacks when windows close or the server changes.
- **Every change checks for stale documentation** in the same commit: search `AGENTS.md`,
  `README.md`, `ROADMAP.md`, code comments and the settings / picker texts for what changed —
  names, paths, file and package names, settings keys, IPC channels, option labels, behaviour
  and platform notes — and fix every mention, including the manual test checklist. Comments
  describe the code as it is, never how it used to be.

### Code style

- 2-space indent, no semicolons, single quotes, dense aligned one-liners like the surrounding
  code. `picker.html`'s inline script is ES5 (`var`, `function`) — match it there.
- Short comments that explain *why* (platform quirks, Electron/Chromium behaviour), not what.
- Log through `log()` with a `[tag]` prefix (`[venmic]`, `[load]`, `[check]`, `[gpu]`). It prints to
  the console and appends to `userData/logs/main.log` (the previous run in `main.old.log`; each
  capped at 5 MB, rotated, so never more than two files). Only the primary instance writes;
  lines logged before the single-instance lock wait in memory. The helper's stderr (probe and
  shares) is logged line by line as `[helper]`.
- New dependencies need a real reason; the only runtime dependency is `@vencord/venmic`
  (optional, Linux-only; listed in `asarUnpack` because it's a native module).

## Commands

```sh
npm install
npm start               # run the app (DevTools: Ctrl+Shift+I)
npm run dist:linux      # AppImage + deb + rpm → release/ (stages the native helper first)
npm run dist:win        # NSIS installer       → release/ (stages the native helper first)
npm run dist:all
npm run stage:native    # build + stage native/ only
```

The native helper needs Rust (`native/rust-toolchain.toml`) and, on Windows, GStreamer 1.28 MSVC
with its development files (`/TYPE=devel`), plus `pkg-config`. On Linux it links our own FFmpeg
and ships our own Mesa Vulkan drivers: `scripts/deps/ffmpeg.sh` then `scripts/deps/mesa.sh` build
them into `build/deps/` (`stage-native.js` runs them when missing), needing cmake, meson >= 1.4,
ninja, libclang (bindgen), PipeWire's development files, and for Mesa LLVM >= 15 with clang, libclc
and the SPIR-V translator -- on Ubuntu 22.04 `scripts/deps/ubuntu-packages.sh` installs them all,
`scripts/deps/container.sh ffmpeg mesa` builds in such a container. At runtime Linux needs only
what a desktop has (PipeWire, libva, the Vulkan loader, a GPU that encodes: for AMD and Intel the
bundled drivers cover distros that strip H.264). After rebuilding FFmpeg, `ffmpeg.sh` cleans the
`ffmpeg-sys-next` crate (Rust bundles the static libraries into it). Without the build tools
`dist:*` still builds, only without the native share; CI sets `SHARKORD_REQUIRE_NATIVE=1` to fail
instead. For `npm start` just `cd native && cargo build --release` (Linux: after the deps scripts;
Windows: with `PKG_CONFIG_PATH` pointing at GStreamer's `lib\pkgconfig`).

Versioning is SemVer, independent of Sharkord's server version, and stays `0.x` while Sharkord
is alpha (minor = features, patch = fixes). Don't bump `version` by hand in feature branches.
Releases: merge `dev` into `main`, then Actions → Build → Run workflow on `main` with a bump —
CI builds, commits `Release vX.Y.Z` to `main`, tags it, publishes the Release (changelog from
commit subjects, so keep them readable) and merges the release commit back into `dev`.
`@vencord/venmic` is an `optionalDependency` because it's Linux-only (`"os": ["linux"]`);
`npm ci` on Windows skips it.

Hardware encoding tip for testing screen share: pick **H264** and turn **Simulcast off** in
Sharkord's Devices settings (with simulcast on, and allowed by the server, Sharkord shares VP8,
which most GPUs can't encode; the native share needs it off too, unless the server disables it). Check
`chrome://gpu` or the `[gpu]` log lines for VA-API status (on Linux Chromium's path encodes on the
GPU only with *Hardware encoding for other shares* on, after a restart). To confirm hardware encoding, look at
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
All four flags are experimental: see ROADMAP.md #88-#91 before relying on or changing them. On Linux
the only flag is `AcceleratedVideoEncoder`, off by default (see ROADMAP.md, Linux flags).

## Manual test checklist

There are no automated tests. After a change, check what it touches:

- **First launch** (delete `serverUrl` from `settings.json`): invalid URL, a non-Sharkord
  site, an unreachable host (error shown, nothing saved), a valid server, Cancel (quits).
- **Change server**: ☰ server menu item, login-screen button, `Ctrl+Shift+O`, tray →
  **Change Server…**; Cancel returns to the current server.
- **Desktop settings**: user Settings → **Desktop Client** (after Others; not in server settings). Picking it
  shows our card in place of Sharkord's, with only Desktop Client highlighted; changes show Sharkord's
  "You have unsaved changes / Save Changes" bar and apply only on Save. Leaving with unsaved changes
  (another entry, the back button, Escape) asks first; Desktop Client doesn't open while Sharkord's own
  tab has unsaved changes. On a narrow window the drawer closes after picking it.
  *Open at login* creates/removes the autostart entry; *Start minimized* is greyed out while it's
  off, and with both on a login launch (`--hidden`) starts in the tray; *Minimize
  to tray* makes X hide the window, tray click reopens it, tray **Quit** exits. The tray menu
  applies the same toggles at once. Launching again focuses the running window.
  *Hardware encoding for other shares* is saved at once but applies at the next launch: the
  `[flags]` lines show it (`on|off (setting|default|env)`) and the features, and with it on a
  Chromium-path share's encoder is the GPU's (not `OpenH264`); a `--disable-features=...` on the
  command line removes a feature from the list.
- **Notifications**: enable them in Sharkord → Settings → Notifications; a message from another
  account while the channel isn't open (or the window is hidden) shows a native notification,
  flashes the taskbar until focused (X11; Wayland ignores it), and clicking it brings the window
  back (also from the tray) and opens that channel or DM — test from the channel list and from the
  DM list. On Windows (installed build) the toast is labelled Sharkord.
- **Unreachable page**: stop the server → Retry and Change server both work.
- **Log files**: launch from the menu (no terminal): `logs/main.log` in the settings folder has the
  `[flags]`, probe and `[gpu]` lines; a restart moves it to `main.old.log`; a second launch while
  running leaves both untouched; `GST_DEBUG=9` never leaves more than the two files, each <= 5 MB.
- **Theme**: switch Sharkord to light, restart with the server down — local pages and the
  picker should be light too.
- **Screen share picker**: on X11 (source grid) and Wayland (portal, then audio step); cancel
  at each step denies the share cleanly.
- **Share audio (Linux)**: per-app and Entire System; the other side must not hear their own
  voice echoed; audio unlinks when the share stops or the picker is closed.
- **Share audio (Windows)**: "Stream With Audio" loopback.
- **Native screen share (Windows, AMD)**: turn it on in Settings → Desktop Client (or the tray). Share a
  screen with H.264, then AV1, simulcast off: a viewer gets 60 fps, rejoining shows a picture
  within a second, and Sharkord's stats show `GPU (Native: AMF, amf…)`. The `[native-share] sent …` lines (DevTools console, and the app's log,
  `logs/main.log`, as `[page] [native-share] …`) should keep `lost`/`resync` near zero
  (`pli`/`fir`: keyframe requests from viewers or mediasoup; `keyreq`: helper keyframes asked for;
  `native keys [<KB> rate|req|other]`: the helper's keyframes and why), and an `estimate fell` line
  marks a collapse of Chromium's estimate with the keyframe before it. `resynced after <why>: <ms>,
  <n> frames not sent` reports each loss of the frame swap's sync (a Chromium keyframe for a request,
  frames Chromium dropped) and how long it took to get back.
  While watching, `[native-share] incoming <codec> <size>: decoder …, GPU|CPU` names the decoder of
  each incoming video (once, and again if it changes); Chromium only names it while the page
  captures (the mic in a voice channel). Every 10 s `[native-share] watching …` gives what the viewer
  got: fps, dropped, freezes, jitter buffer, keys, pli, lost, nack, kbps. A window share, VP8, or
  the setting off must behave exactly as before.
- **Native screen share (Linux Wayland; AMD, Intel)**: the switch is usable only when the startup
  probe passes (`[native-share] probe` in the log, with `api` and `driver`); otherwise it's greyed
  out with the reason, and "This GPU can hardware encode" lists H.264 / AV1. Share with H.264:
  **one** portal dialog then the audio step, the local preview moves, a viewer gets 60 fps, stats
  show `GPU (Native: Vulkan (hardware), h264_vulkan)`, and the `sent` lines show `native keys` only
  on request (`req`) and none after a bitrate change. At 4K a still screen then motion: no storm of
  keyframe requests on the viewer (`watching … pli` near 0). On a clean Fedora install (no RPM
  Fusion drivers) the probe says `"driver":"bundled"` and H.264 still works. Cancelling the portal dialog or the audio step cancels the share
  and the desktop's sharing indicator goes away; stopping the share ends the helper. Suspending
  mid-share ends the share, and the next share is native again. A screen with fractional scaling
  (a 4K panel at 160%) shares at its physical size: the `started` line's `source` and `size` say
  3840x2160, not 2400x1350 (with Sharkord's resolution at 4K).
- **Codec routing (both platforms)**: VP8, VP9, `auto`, or Simulcast on (on a server that allows
  it) in Sharkord's Devices settings, and a codec the probe didn't open (AV1 on a GPU without AV1 encoding), are Chromium's share: its
  picker, a moving preview, no `sharkord-share` process and Chromium's encoder in the stats.

## Commits and privacy

This repo is public: `agrisci/sharkord-client`.

- **No AI attribution** — no `Co-Authored-By: Claude` or similar trailers in commits or PRs.
- Never rewrite published history (including the `Initial commit`): no force-pushes to `dev` or `main`.
- Keep personal info out of code, docs and commits: no personal emails, no real server URLs
  (use `https://sharkord.example.com`).
- Branches: work on a feature branch off `dev` and open the PR against `dev` (not `main`);
  don't commit straight to `dev`. **Open the PR only when the branch is ready to merge**: CI
  builds both platforms for every push to a branch with an open PR (a newer push cancels the run
  still going for the older commit), so an early PR spends a build on each work-in-progress commit. Test locally until then (`npm start`, `cargo build`,
  `npm run dist:*`); the PR's CI run then checks the final state, and must pass.
- Commit messages: short imperative subject, blank line, `-` bullet body describing each change.
