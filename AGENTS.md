# AGENTS.md

Guide for AI agents working on the Sharkord desktop client. See [README.md](README.md) for
what the app does from a user's point of view and [CONTRIBUTING.md](CONTRIBUTING.md) for building
and releasing — this file covers how the code is organized.

This is a thin Electron shell around a self-hosted [Sharkord](https://github.com/Sharkord/sharkord)
server: it loads the server's own web app and adds only what a browser can't do — a screen
share picker with share audio, hardware video encoding, and a way to pick/change the server.
There is no build step, no TypeScript and no bundler: plain CommonJS JavaScript and static HTML.

Core principle: **no over-engineering**. Follow the existing pattern, add the smallest thing
that works, and don't introduce abstractions or dependencies for a single use case. The Electron
side is ~3000 lines and the native helper ~3300 (`native/`, `scripts/stage-native.js`, `scripts/deps/`), on purpose.

## Architecture

| Path                         | What it is                                                                                   |
| ---------------------------- | -------------------------------------------------------------------------------------------- |
| `electron/main.js`           | Main process: settings, Chromium flags, venmic, screen picker, native share helper (probe, spawn, frames), main window, server check, first launch / change server, open at login + tray, updates (electron-updater), diagnostics (the redacted report file), lifecycle |
| `electron/preload.js`        | Main window preload: `electronAPI` bridge, `getDisplayMedia` hooks (share audio; the native share's placeholder swap, frame worker and decoded preview), `Notification` hook (taskbar flash; click shows the window and opens the channel/DM), the page's visibility (`document.hidden` from the window, see *Notifications* below), the page-side log lines (`[notify]`, `[ws]`, `[hook]`, `[page] error:`), injected "Change server" controls, a **Desktop Client** tab in the user settings (the options and a Diagnostics card with **Save diagnostics…**) and the update arrow in the header |
| `electron/picker.html`       | Screen share picker (source grid + audio step), styled like Sharkord                          |
| `electron/picker-preload.js` | `pickerAPI` bridge for the picker window                                                      |
| `electron/first-launch.html` | Server URL prompt (first run and Change server)                                               |
| `electron/unreachable.html`  | Shown when the server can't be reached or isn't a Sharkord server                             |
| `electron/theme.css`         | Sharkord's design tokens + card/input/button styles for the two pages above                  |
| `build/`                     | Icons packaged by electron-builder (committed so CI can build); `build/native/` is staged, not committed |
| `native/`                    | Native screen share helper (Rust + our static FFmpeg, `src/ffmpeg.rs`: encoder and paced encode loop): captures a monitor (Windows: DXGI desktop duplication → AMF, NVENC or Quick Sync, `src/windows.rs`) or a portal pick (Linux Wayland: PipeWire → Vulkan video or VA-API, `src/linux/`), frames on stdout (see *Native screen share*) |
| `scripts/stage-native.js`    | Builds the helper and stages it into `build/native/` (shipped as `resources/native/`): on Windows one static exe, on Linux with the bundled Mesa Vulkan drivers (`mesa/`); licences in `LICENSES/` |
| `scripts/deps/`              | The helper's FFmpeg, static LGPL with our patches (`ffmpeg-*.patch` Linux, `ffmpeg-windows-*.patch` Windows): `ffmpeg.sh` (Linux), `ffmpeg-windows.sh` (Windows, MSVC from MSYS2's bash); Linux also `mesa.sh` (RADV/ANV with H.264 encode, the helper's fallback), `ubuntu-packages.sh` (their build dependencies, CI), `container.sh` (the same in Ubuntu 22.04 via podman) |
| `.github/ISSUE_TEMPLATE/`   | The bug report template: asks for the diagnostics file                                       |
| `.github/workflows/build.yml` | CI: builds on Linux + Windows runners for PRs into `dev`/`main` and pushes to `dev` (artifacts); run manually on `main` with a version bump to release |
| `upstream/`                  | Gitignored local clones for reference only (`sharkord-src`, `Sunshine`, ...) — never edit or import |

`main.js` is split into sections with `// ── Name ──` banners; keep new code in the section it
belongs to rather than adding files.

## How the pieces talk

- **Settings**: `userData/settings.json` (`~/.config/sharkord`, `%APPDATA%\sharkord`) via `loadUserSettings` / `saveUserSettings`. Keys:
  `serverUrl` (saved without a trailing slash — use `savedServerUrl()`), `theme`
  (`'dark' | 'light'`, remembered from the page so local pages match it), `audio` (venmic
  options, merged over `AUDIO_DEFAULTS`), `minimizeToTray` (default off), `startMinimized` (a login launch stays in the
  tray), `nativeShare` (unset: on where the probe's encoder has been tested -- AMF, Vulkan video,
  VA-API -- off with NVENC and Quick Sync until they are (`nativeShareTested`); a choice saved in
  Settings or the tray wins either way; usable only when the helper's `--check` probe at startup encodes with a codec -- AMF, NVENC
  or Quick Sync on Windows, Vulkan video or VA-API on Linux Wayland -- and a share goes native only with a codec it found; where it
  can't run, its switch is greyed out and `nativeShareNote` says why, and `nativeShareCodecs` lists
  what the probe found the GPU hardware encodes, H.264 / AV1 with a check or a cross),
  `chromiumHwEncode` (hardware encoding for shares on Chromium's own path, i.e. the Chromium flags
  below; default on on Windows, off on Linux; read once at launch, `SHARKORD_CHROMIUM_DEFAULTS=1`
  forces it off), `autoUpdate` (default on; see *Updates* below; `autoUpdateNote` is the state line
  the tab shows, or why the updater can't run here, in which case the key is left out and the switch
  greyed out). *Open at login* is not
  stored: the OS login item / `~/.config/autostart/sharkord.desktop` is the source of truth.
  Always spread the existing settings when saving.
- **IPC** (`ipcMain.handle` / `ipcRenderer.invoke` unless noted):
  - Page → main: `virtmic-active`, `virtmic-unmute`, `virtmic-stop`, `change-server` (`send`),
    `desktop-settings-get`, `desktop-settings-set`, `notification-shown` (on Windows with the
    notification for main to show) / `notification-clicked` / `notification-skipped` (`send`), `native-share-pick` (Linux) and `native-share-target` (both with the share's codec),
    `native-share-start` / `native-share-stop` (`send`), `update-get`, `update-check`,
    `update-install` (`send`), `diagnostics-save` — only accepted from the main window's webContents.
  - Main → page: `native-share-port` (a `MessagePort` tagged with the share's `id`, forwarded
    into the page world with `window.postMessage`): helper frames and events one way,
    `keyframe`/`bitrate`/`stop` the other; `update-state` (the updater's `{ status, version,
    percent, needsPassword, note, notesUrl }` on every change; `notesUrl`, the release's page while
    one downloads or is ready, is the *What's new* link in the tab and the arrow's dialog);
    `window-visible` (a boolean on every show, hide, minimize, restore, focus, blur and page load, forwarded into
    the page world with `window.postMessage`); `notification-click` (Windows: the id of a toast main
    showed, forwarded the same way, so the page opens its channel).
  - Picker → main: `virtmic-list`, `audio-settings-get`, `audio-settings-set`,
    `picker-go-live`, `picker-cancelled` (`send`).
  - Main → picker: `init` (sources, `skipPicker`, `platform`, `theme`).
- **First-launch window** has no preload. It reports back through `console-message`:
  `cancel` or `form:{json}` (same channel Vesktop's first-launch view uses). Main answers
  with `executeJavaScript('setError(...)')`.
- **Notifications** show exactly when Sharkord's window isn't focused (hidden to the tray,
  minimized, behind another app), the open channel included. Sharkord notifies (`new
  Notification`) for a channel that isn't open, or any channel while `document.hidden` is true;
  with `backgroundThrottling` off (voice keeps running in the tray) Chromium never marks the page
  hidden, so main sends `window-visible` and the page's `installVisibility` shadows
  `document.hidden` / `visibilityState` with it and fires `visibilitychange`. Hidden means hidden,
  minimized **or unfocused**: a minimize by the compositor never reaches Electron on Wayland (no
  event, `isMinimized()` false), focus does. Sharkord reads it only for notifications (and its voice
  debug log). While the window is focused the hook skips the notification Sharkord makes for
  another channel (`[notify] skipped: the window is focused`); its sound and unread badge stay.
  `[window] page visibility:` is logged once a state has held 1 s (switching windows leaves none).
  **On Windows main shows the toast** (Electron's `Notification`): Electron makes Chromium's
  notification id (`n#<origin>#<hash>`) the toast's tag, which Windows caps at 64 characters, so
  with a server address over ~29 characters every toast was refused, silently (the page gets no
  `error`). The page hook hands the title and body to main instead (`| by main`), whose toast gets
  a UUID; its click comes back as `notification-click`. A toast leaves the notification centre when
  its popup times out, and any on screen is closed at quit: there is no COM activator, so a click
  there after a quit would find the app gone. Electron's `show` only means Windows took it; a
  refusal (Windows' switches for all notifications or Sharkord's) is `[notify] error:` with
  Windows' reason. Not packaged, the AppUserModelID is `com.sharkord.client.dev`: Electron's first
  toast leaves a Start menu `Electron.lnk` with the ID, after which Windows labelled the installed
  build's toasts Electron (and the login item, named after the ID, was the dev build's). Linux keeps Chromium's, where `[notify] no show after 10 s` marks one dropped
  without an event. Logged on both ends: `[hook] notifications:` (permission and Sharkord's switches, from its
  localStorage keys `sharkord-browser-notifications*`) once per load, `[hook] switch …` on a
  change, `[notify] shown:` from the page (permission, hidden, focus, switches) and from main
  (window state, flash), the OS's `show`/`error`/`close` (Windows: from main, `close:` with the
  reason), and `[notify] click:` (dm or channel,
  item found, sidebar switched), never the title or body.
- **Unreachable page** buttons are links to `sharkord://retry` and `sharkord://change-server`,
  intercepted by the main window's `will-navigate`.
- **Local pages** get their state through `loadFile(..., { query })` (`theme`, `url`,
  `current`, `reason`) and apply the theme class before first paint.
- **Tray / no menu bar**: there is no application menu (`Menu.setApplicationMenu(null)`);
  shortcuts are handled in `before-input-event`. The tray menu (Open, Change Server…, Open at
  login, Start minimized, Minimize to tray, Quit) is the non-DOM fallback. With *Minimize to tray*
  on, `close` hides the window unless `quitting`. A notification shown while the window isn't
  focused puts a red dot on the tray icon (`setBadge`, drawn by `withDot` into the icon's bitmap;
  Wayland ignores the taskbar flash) and on Windows on the taskbar button (`setOverlayIcon`; the tray
  icon often sits in the overflow), cleared when the window is focused (`[tray] badge on|off`). Autostart launches with `--hidden`, which only
  marks a login launch: it starts in the tray when *Start minimized* is on (independent of
  *Minimize to tray*, like Vesktop's). A single-instance lock makes a second launch show the window.
- **Server check**: `checkServer` fetches `<url>/info` and expects `serverId` and `name`
  strings → `'ok' | 'not-sharkord' | 'unreachable'`. `loadServer` checks before loading —
  navigating to an unreachable URL and then to a local page can leave the window unable to paint.
- **Updates**: `electron-updater` against this repo's GitHub Releases: a check 15 s
  after launch, every 4 h and 30 s after a resume from suspend (`checkForUpdates`; off with
  `autoUpdate` false, a manual check from the tab's button or the tray still works; while a native
  share runs an automatic check is postponed 10 min, one at a time), the download in the background, then a green
  `circle-arrow-down` cloned from the ☰ server-menu button (`server-menu-trigger`) in Sharkord's
  header, a *Restart to update to vX* tray item and the tab's state line, until the user restarts
  into it (`quitAndInstall(true, true)`: silent, relaunch). The feed comes from
  `resources/app-update.yml` (electron-builder writes it from `build.publish`) and the
  `latest.yml` / `latest-linux.yml` + `*.blockmap` CI uploads next to the installers (a CI step
  fails the build when they are missing; the
  release's `files` lists AppImage, deb and rpm; a prerelease version publishes its own channel,
  `beta.yml`, that stable installs ignore). Per install: NSIS runs the installer with `--updated /S`;
  the AppImage is replaced in place (renamed to the new version's name when the old one had a
  version, so `appimage-filename-updated` rewrites the autostart entry; a name without a version,
  `Sharkord.AppImage`, is kept, which the README recommends for shortcuts); deb and rpm go through
  `pkexec dpkg -i` / `dnf install`, a password dialog (`needsPassword`), so `autoInstallOnAppQuit` is
  on only for Windows and the AppImage; they run synchronously in the main process (measured ~12 s
  for the rpm), so `installUpdate` hides the window first and the error handler shows it again if
  the dialog is cancelled (the note then says the install failed). After a deb/rpm install the new
  version is started by our own detached `spawn` (`SHARKORD_RELAUNCHED=1`), not electron-updater's
  `app.relaunch()`: Electron relaunches through a helper that sets no-new-privileges, under which
  pkexec can't elevate ("pkexec must be setuid root"), so the *next* update would have failed.
  Relaunched that way, or by electron-updater's AppImage install (`APPIMAGE_SILENT_INSTALL`), the
  single-instance lock is retried for 5 s while the old process quits. Not packaged, or an AppImage run from its extracted files:
  the updater is off with the reason in `autoUpdateNote`. Unsigned Windows builds: the updater skips
  the Authenticode check without a `publisherName` and trusts `latest.yml`'s SHA-512.
  30 s after startup (the NSIS installer is still running from it) the installer of the running version is removed from electron-updater's cache
  (`sharkord-updater/pending` in `%LOCALAPPDATA%` or `~/.cache`); a newer pending download is kept.
  `SHARKORD_UPDATE_FEED=<url>` (installed builds) replaces GitHub with a directory of
  `latest*.yml` + installers, to test the whole flow offline, `SHARKORD_UPDATE_INTERVAL=<ms>` shortens
  the periodic check; `SHARKORD_TEST_UPDATE=install|quit`
  restarts into a downloaded update at once, or quits (the install-on-quit path). Logged as
  `[update]`. The tab's Updates card shows the client's version (`version` in `desktop-settings-get`).
- **Diagnostics**: the tab's Diagnostics card has **Save diagnostics…** (`diagnostics-save` →
  `saveDiagnostics`): a save dialog, then one text file (default `sharkord-diagnostics-<date>-<time>.txt`
  in Downloads) with a header naming the replacements, the `[sys]` lines, `SHARKORD_*` env, the
  Chromium flags (read back from `app.commandLine`), GPU status, the helper's probe, the updater's
  state, a State section (the window's state and tray, the startup options, Electron's
  `Notification.isSupported()`, and `pageState()`: the page's notification permission,
  `document.hidden`, focus, `navigator.onLine` and Sharkord's four notification switches, read with
  `executeJavaScript` within 2 s; the notifications shown, skipped, clicked and failed this run, with
  the badge; on Windows its switches for all notifications and Sharkord's, `windowsToasts()` from the
  registry), the settings without `serverUrl`, and `main.log` then
  `main.old.log` as they are on disk.
  `redact()` is the safety net on top of what the log never gets (below): the server URL, its host
  and hostname → `<server>`, the paths again, and the user name as a whole word (3+ chars, any
  case) → `<user>`. The hostname is left alone (nothing logs it; `fedora` as a hostname would
  mangle unrelated lines). Returns the path, `null` cancelled, `false` failed (the card's note says
  so). Only in the tab, not the tray (it looked out of place there); the README keeps the log
  path as the manual way. `SHARKORD_TEST_DIAGNOSTICS=<file>` writes the file there 8 s after
  startup without the dialog, to check its content. The card's one-line description promises a
  redacted log; the file's header and the README say what is replaced: keep them true when adding
  log lines.

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

### Native screen share (Windows, Linux Wayland)

With the `nativeShare` setting on (by default where the encoder is tested, see *Settings*; or
`SHARKORD_NATIVE_SHARE=1`), some shares are captured and encoded outside Chromium by the helper in
`native/`, while Sharkord and Chromium keep everything else (connection, packetization, bandwidth
estimate). **Chromium's own share takes every share the helper isn't sure of**: the helper is
chosen per share, so nothing it can't handle ever reaches it.

**Which path a share takes.** At startup `probeNativeShare` runs the helper's `--check` once: it
encodes a frame with each codec (Windows: AMF, else NVENC, else Quick Sync, on the first adapter
that encodes; Linux: Vulkan video, else VA-API, on the system's drivers or the bundled ones,
reported as `driver`), reports the `api` and `device`, and `h264`/`av1`. No codec →
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

| | Windows (AMD, NVIDIA, Intel) | Linux (Wayland; AMD, Intel) |
|---|---|---|
| The rule is applied | after the pick (`native-share-target`, with the codec) | before the pick (`native-share-pick`, with the codec) |
| Pickers | our source grid, then the audio step | the helper's portal dialog (once), then the audio step |
| Capture | helper **and** Chromium's own, still running | helper only |
| Local preview | Chromium's capture | the helper's frames, decoded in the page |
| Graph | `ddagrab` (a screen) or `gfxcapture` (a window), D3D11 → `scale_d3d11` → `h264_amf` / `av1_amf` (NVENC, QSV; below) | PipeWire (DMA-BUF) → `scale_vulkan` → `h264_vulkan` / `av1_vulkan` (below) |
| Windows (app windows) | helper (`window:<HWND>` from our picker) | helper (the portal offers them) |

**When it fails anyway** (the `s.fallback` cases; a share is never left on the black placeholder):

| Failure | Windows | Linux |
|---|---|---|
| Before the first native frame (the encoder won't start despite the probe, a codec other than the settings said) | before the placeholder reached a connection, the share goes on with Chromium's capture; after it, the share **ends** as mid-share (swapping the capture in then left viewers black, #87) | the share **ends** (nothing to fall back to); later shares that session are Chromium's |
| Mid-share (helper error or exit, watchdog: no frame for 10 s / none swapped for 6 s) | the share ends; later shares that session are Chromium's (logged at each share), unless the system was suspended during it (`powerMonitor`, sent as a `suspend` event on the share's port) | same |
| A picker or the audio step cancelled | no share | no share; the helper is stopped |

A share ends through `ended` on Sharkord's track, sent again every second until Sharkord stops it
(at the very start Sharkord isn't listening yet).

**Windows details.** The helper (`native/src/windows.rs`) links a static FFmpeg of our own
(`scripts/deps/ffmpeg-windows.sh`: MSVC, the static C runtime, AMF and NVENC headers, libvpl for
Quick Sync), so it is one exe (1.6 MiB; the GStreamer subset it replaced was ~18 MiB):
- **Monitor**: DXGI's adapters and outputs, matched to the pick by Windows' friendly name (what
  Electron's `Display.label` is, from `DisplayConfigGetDeviceInfo`) and primary. Capture, conversion
  and encoder share one D3D11 device on the adapter driving that monitor (duplication only works
  there; a hybrid laptop's cross-adapter case is ROADMAP #11).
- **A window** (`nativeTargetFor` passes its HWND): `gfxcapture` (Windows.Graphics.Capture, FFmpeg
  9), on the adapter of the monitor it is on, labelled with its title; a picture only when it
  changes, upright whatever the monitor's rotation. The
  stream follows the window's size: `gfxcapture` keeps its first size (scaling a resized window into
  it, pinned left), so the capture is rebuilt once a new client size has held for 500 ms (checked
  every 250 ms, not while minimized). A minimized window sends nothing and FFmpeg's pull waits for
  a frame with no way to interrupt it, so on stop the helper waits for its capture thread 1.5 s at
  most, then exits without it. A closed window ends the share once the capture retries give up.
  FFmpeg's `-lstdc++` for it is skipped in `build.rs` (MSVC's static CRT has the C++ runtime).
- **Capture** of a screen: `ddagrab`, a picture every tick (its repeats are skipped). It polls DXGI at each tick
  (our `ddagrab-poll` patch): `AcquireNextFrame` holds the device's unfair lock while it waits
  (Sunshine found the same), which starved the converter and AMF -- the encode loop ran at 39-53 fps,
  now 60. A lost capture (UAC prompt, mode change, fullscreen game) is rebuilt, retried for 5 s
  while the encoder repeats the last picture. Always 8-bit BGRA (DXGI converts an HDR desktop).
- **Conversion**: `scale_d3d11` to NV12 at the fitted size, BT.709 limited range (our patch: it set
  no colour space, and wrote every frame into the same texture), turned upright for a rotated
  monitor in the same pass (our patch's `rotate`: DXGI hands a portrait 1080x1920 over as
  1920x1080, `ddagrab` only turns the cursor). A texture per frame: AMD refuses
  NV12 render target arrays; a fixed pool only for Quick Sync, which maps it (`hwmap`).
- **Encoders**, one tried after the other on that adapter: `h264_amf`/`av1_amf` (Sunshine's options:
  ultra-low-latency, `vbr_latency`, one frame in flight, forced IDRs, no frame delay with FFmpeg 9's
  `AV_CODEC_FLAG_LOW_DELAY`), `h264_nvenc`/`av1_nvenc`, `h264_qsv`/`av1_qsv` (NVENC and QSV built
  in, untested: ROADMAP #7, #8). AMF's rate control ignores the VBV, HRD and `max_au_size` on an RX
  9060 XT: after a mostly still screen a scroll came out as a 1.2 MB frame, larger than an IDR of
  the same picture. A floor under the quantizer caps it, following the rate (our AMF patch changes
  it in place): QP 18 at 0.05 bits per pixel (25 Mbps at 4K60; scroll bursts 297 KB), 6 more per
  halving of the rate (AV1: q-index 60, 24 more) -- a fixed 18 made 600 KB keyframes at the ~6 Mbps
  a share starts with (8 keyframe requests in 15 s). It also makes a lower rate take hold at once:
  without it AMF took over 2 s to follow a cut. FFmpeg's default `qmax` (31) is lifted to the
  codec's maximum: it reached AMF as its maximum QP, and 4K motion then couldn't get under 16-20
  Mbps. H.264 is **High profile with CABAC** (`640433`), sent under Sharkord's Baseline label
  (`42e01f`): decoders go by the stream's SPS, as they already did for its level (5.1, not 3.1);
  the laptop decodes it on its GPU at 60 fps. Measured with VMAF on a 4K desktop (`h264_amf`, the
  helper's settings): the same quality as Constrained Baseline at 34% fewer bits on scrolling text,
  and better quality at 39% fewer on video; at 3 Mbps text's 1% low went 63 -> 85. AMF's `quality`
  preset and pre-analysis made it worse or slower (57-67 fps). `SHARKORD_H264_PROFILE=baseline`
  goes back to Constrained Baseline (`424033`, the constraint flag set: #86). NVENC and Quick
  Sync stay Constrained Baseline until tested.
- **Pacing and bitrate** as on Linux (`src/ffmpeg.rs`): our patches change AMF's, NVENC's and Quick
  Sync's rate in place, without a keyframe. `cargo test -- --ignored` encodes this desktop
  (scrolled every other frame): keyframes on request only, ≤ 600 KB, a bitrate change without one.
  `SHARKORD_ENCODE_API=amf|nvenc|qsv` forces an API. `SHARKORD_TEST_FAIL=start` makes the helper
  fail its share at once, `=<ms>` send an `error` that long after starting (both platforms), to
  test the fallback.

**Linux details.** The page's `getDisplayMedia` hook calls `native-share-pick` instead of
Chromium's; main spawns the helper, which opens its own ScreenCast portal session at once
(`native/src/portal.rs`, `ashpd`; monitors and windows, nothing persisted) and answers `selected`
or `cancelled`, then main shows the picker's audio step. The page builds Sharkord's stream itself:
a `MediaStreamTrackGenerator` fed with the helper's frames decoded by `VideoDecoder`, and venmic's
virtual mic (`withShareAudio`).

The Linux helper (`native/src/linux/`): `capture.rs` reads the portal's PipeWire stream
and `encode.rs` converts for the encoder (`src/ffmpeg.rs`), all with a static FFmpeg of our own
(`scripts/deps/ffmpeg.sh`):
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
  keyframes only on request (no GOP), no B-frames, H.264 Constrained Baseline with CAVLC, BT.709,
  AV1's level set from the picture size (`av1_vulkan` otherwise writes 2.0, and Chromium's keyframe
  check -- libgav1, in the preview's `VideoDecoder` -- refuses anything over 512x288). Measured offline at 4K60 21 Mbps (still desktop, scrolling text, Big Buck
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
- **Drivers**, in this order: the system's Vulkan; the bundled Vulkan drivers; the system's VA-API.
  Fedora and openSUSE build Mesa without H.264 (VA-API and Vulkan), so the package ships RADV and
  ANV built with it (`scripts/deps/mesa.sh`, `resources/native/mesa/`): when the system's Vulkan
  can't encode H.264, the helper re-runs itself with the Vulkan loader pointed at them
  (`VK_DRIVER_FILES`, `SHARKORD_BUNDLED_DRIVER=1`) -- ahead of a system VA-API that could (RPM
  Fusion's), because VBR under the frame cap beats VA-API's CBR. VA-API stays for GPUs Mesa's Vulkan
  can't encode on (AMD before VCN, Intel before Gen12). Only the helper loads the bundled drivers.
  NVIDIA needs NVIDIA's own driver (nouveau can't encode). VBR aims 6% under the target (it runs
  that much over).
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
   For H.264 the helper pins its profile (FFmpeg's `profile`: High on AMF, Constrained Baseline
   elsewhere, under Sharkord's `42e01f` either way) and reports the first keyframe's SPS as
   a `stream` event, so the log shows what viewers really get.
4. Each helper frame gets one placeholder frame (paced: Chromium thins out bursts), and the worker
   swaps its payload into the matching outgoing frame. Chromium derives the RTP timestamp from the
   placeholder's, so frames pair by timestamp and size. Keyframes must line up (receivers and
   mediasoup trust Chromium's frame type): a native keyframe changes the placeholder's size,
   which makes Chromium key that exact frame (Chromium 152 has no `generateKeyFrame()` here).
5. The helper's bitrate follows Chromium's transport estimate (`availableOutgoingBitrate`), not
   `targetBitrate`: Chromium counts the bytes the transform adds as post-encode overhead and halves
   the encoder target, and its estimate only grows to 1.5x what is acknowledged. While loss, round
   trip and Chromium's pacer queue stay clean it only steps up, to 0.9x the estimate (which dips
   15-50% every few seconds on a clean LAN with mediasoup; the tenth is room for audio, overhead and
   keyframes on a link that really is that fast); on congestion (loss, a growing round trip, or
   packets waiting over 250 ms in the pacer, i.e. the estimate really below what goes out; a
   keyframe's 50-180 ms doesn't count) it drops to 0.85x.
   **While the screen is still the helper pads** (`padded` in `src/ffmpeg.rs`): frames are filled
   up to 90% of the rate, against a running budget (never over it with the video counted), with
   data decoders skip (an H.264 filler NAL unit; for AV1 a private
   metadata OBU, as Chromium's AV1 packetizer drops padding OBUs), and the page counts it as sent.
   Without it the encoder sends ~0.1 Mbps, Chromium's estimate falls to 0.85x what was acknowledged
   (25 -> 5 Mbps), and after motion resumes it only grows ~8%/s: 15-20 s of lower quality, or, sent
   at the old rate anyway, 1.4 s in the pacer. With it, motion is back at 25 Mbps at once (4K60,
   H.264 and AV1). The cost: a still screen uses the full rate, for the sender and each viewer.
   Tried and dropped: screen-content mode (`contentHint 'detail'`: the estimate still fell, and its
   pacing added 600-860 ms), and a probe by lowering and raising `maxBitrate` (no effect).
   `SHARKORD_PAD=<0..1>` sets the share (0: off); the page still follows the estimate down when the
   helper sends under half its rate, padding included.
   **A frame dropper** keeps the helper at its rate when the encoder can't get under it (AMF at 4K60
   with a video playing needs 2-4 Mbps at its coarsest): once it has sent 250 ms of the rate over,
   it skips ticks (never a keyframe) -- fewer frames at the same resolution, as Chromium does for
   screen content (`held` in the helper's stats). Measured with a 3 Mbps upload cap (a Windows QoS
   policy on `electron.exe`), 4K60 H.264, a video playing: before, the helper sent 2.4-4 Mbps asked
   for 1-2, 1.2-1.4 s in the pacer, the viewer at 22-41 fps with 2.5-5.4 s of freezes per 10 s;
   now 51-58 fps, under 0.8 s, the pacer mostly under 20 ms.
   The helper changes it in place, without a keyframe, so it steps 10% at least 2 s apart; keyframe
   requests go to it at most every 300 ms. Capped by Sharkord's bitrate
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
  `README.md` (for users), `CONTRIBUTING.md` (building and releasing), `ROADMAP.md`, code comments and the settings / picker texts for what changed —
  names, paths, file and package names, settings keys, IPC channels, option labels, behaviour
  and platform notes — and fix every mention, including the manual test checklist. Comments
  describe the code as it is, never how it used to be.

### Code style

- 2-space indent, no semicolons, single quotes, dense aligned one-liners like the surrounding
  code. `picker.html`'s inline script is ES5 (`var`, `function`) — match it there.
- Short comments that explain *why* (platform quirks, Electron/Chromium behaviour), not what.
- Log through `log()` with a `[tag]` prefix, one per area (`[window]`, `[tray]`, `[settings]`, `[app]`,
  `[power]`, `[load]`, `[check]`, `[notify]`, `[screen-share]`, `[venmic]`, `[gpu]`, ...). The page logs with
  `console.log` and our tag; main's `console-message` forwards only `[native-share]`, `[share]`,
  `[notify]`, `[ws]` (Sharkord's WebSocket: connecting, open, close code), `[hook]` (our injected
  controls added, or their anchor missing) and `[window]` lines as `[page] …`, and the page's uncaught
  errors as `[page] error:` (first line, 300 chars, the script's file name, 50 per load), with the
  server replaced (`scrubServer`) in the last two kinds. Every user-facing action and state change
  leaves a line, so a diagnostics file answers a report. It prints to
  the console and appends to `userData/logs/main.log` (the previous run in `main.old.log`; each
  capped at 5 MB, rotated, so never more than two files). Only the primary instance writes;
  lines logged before the single-instance lock wait in memory. The helper's stderr (probe and
  shares) is logged line by line as `[helper]`. Two `[sys]` lines at startup (`sysLines()`, also
  in the diagnostics file): version and packaging, Electron/Chromium/Node, OS, locale, CPU, RAM,
  display sizes, session type and desktop, whether the helper is there.
  **The log never identifies the user**, as it goes into bug reports as it is: `log()` replaces
  the settings and home folders in every line (`<userData>`, `<home>`; any separators, any case
  on Windows), and no line carries the server or a page URL (`[check]`, `[load]` log only the
  error), window titles (a window share's `started.monitor` becomes `<window>` before it is
  logged or sent to the page), the apps picked for share audio (a count), notification titles or
  bodies, channel, DM or user names, a WebSocket close's reason (a moderator's text: its length),
  hostnames or usernames. Log what helps (versions, hardware, states, errors), never what names someone.
- New dependencies need a real reason; the runtime dependencies are `@vencord/venmic`
  (optional, Linux-only; listed in `asarUnpack` because it's a native module) and
  `electron-updater` (updates; electron-builder's own, and it knows NSIS, AppImage, deb and rpm).

## Commands

```sh
npm install
npm start               # run the app (DevTools: Ctrl+Shift+I)
npm run dist:linux      # AppImage + deb + rpm → release/ (stages the native helper first)
npm run dist:win        # NSIS installer       → release/ (stages the native helper first)
npm run dist:all
npm run stage:native    # build + stage native/ only
```

The native helper needs Rust (`native/rust-toolchain.toml`), `pkg-config` and libclang (bindgen;
Windows: `winget install LLVM.LLVM`). On Windows it links our own FFmpeg:
`scripts/deps/ffmpeg-windows.sh` builds it into `build/deps/` with Visual Studio 2022's C++ tools,
run from MSYS2's bash (`winget install MSYS2.MSYS2`, then `pacman -S make diffutils git pkgconf`;
`stage-native.js` runs it when missing). On Linux it links our own FFmpeg
and ships our own Mesa Vulkan drivers: `scripts/deps/ffmpeg.sh` then `scripts/deps/mesa.sh` build
them into `build/deps/` (`stage-native.js` runs them when missing), needing cmake, meson >= 1.4,
ninja, libclang (bindgen), PipeWire's development files, and for Mesa LLVM >= 15 with clang, libclc
and the SPIR-V translator -- on Ubuntu 22.04 `scripts/deps/ubuntu-packages.sh` installs them all,
`scripts/deps/container.sh ffmpeg mesa` builds in such a container. At runtime Linux needs only
what a desktop has (PipeWire, libva, the Vulkan loader, a GPU that encodes: for AMD and Intel the
bundled drivers cover distros that strip H.264). After rebuilding FFmpeg, `ffmpeg.sh` cleans the
`ffmpeg-sys-next` crate (Rust bundles the static libraries into it). Without the build tools
`dist:*` still builds, only without the native share; CI sets `SHARKORD_REQUIRE_NATIVE=1` to fail
instead. For `npm start` just `cd native && cargo build --release` after the deps scripts.

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
the `media-source` `framesPerSecond` with the encoder's); it is only enabled with a single hardware
GPU, as texture capture fails for good if the GPU adapter changes (hybrid laptops, #90). AV1 uses
`WebRtcAV1HWEncode`, and where an AMD GPU is present `ExpandMediaFoundationEncodingResolutions` is
disabled, because Chromium drops AMD's AV1 encoder when it can't query its resolutions. That
feature is on by default in Chromium; turning it off caps every hardware encoder at 1080p, so on
AMD 1440p/4K shares on Chromium's path fall back to software (`OpenH264` / `libaom`); NVIDIA and
Intel keep it (#88). The GPUs come from one registry query before `ready` (`gpuVendors`, the
`[flags] GPUs:` line; virtual adapters don't count; unreadable: the old unconditional flags).
NVIDIA never gets hardware Constrained Baseline H.264 in Chromium (crbug 1088650).
All four flags are experimental: see ROADMAP.md #88-#91 before relying on or changing them. On Linux
the only flag is `AcceleratedVideoEncoder`, off by default (see ROADMAP.md, Linux flags).

## Manual test checklist

There are no automated tests. After a change, check what it touches:

- **First launch** (delete `serverUrl` from `settings.json`): invalid URL, a non-Sharkord
  site, an unreachable host (error shown, nothing saved), a valid server, Cancel (quits).
- **Change server**: ☰ server menu item, login-screen button, `Ctrl+Shift+O`, tray →
  **Change Server…**; Cancel returns to the current server.
- **Desktop settings**: user Settings → **Desktop Client** (after Others; not in server settings). Picking it
  shows our cards (Startup and Tray, Screen Sharing, Updates, Diagnostics) in place of Sharkord's, with only Desktop Client highlighted; changes show Sharkord's
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
- **Updates** (installed builds; `npm start` shows the switch greyed out with "Updates need an
  installed build"; the Updates card names the running version): build the current version, bump `package.json` and build again into another
  directory, serve that one (`python3 -m http.server`) and start the older build with
  `SHARKORD_UPDATE_FEED=http://localhost:8000`. The log shows `[update]` checking, downloading and
  "ready"; the tab's note follows (empty until the first check; the button `Check for updates`, a filled `Restart to install X` once downloaded; neither shows the save bar), the tray gets
  *Restart to update to vX*, and a green arrow appears before ☰ in the header (not on the login
  screen). Clicking any of them restarts into the new version (the arrow asks first, in a dialog like
  Sharkord's; Cancel keeps it): on Windows and the AppImage without a further prompt (and quitting instead installs it too; the AppImage is renamed and, with *Open at login* on,
  the autostart entry points at the new file); a deb or rpm asks for the password with the window hidden (cancelling it
  brings the window back with the update still ready), and never installs on quit. Then update
  **again** from the relaunched app (a third build on the feed): the password dialog must appear and
  the install go through (`NoNewPrivs: 0` in `/proc/<pid>/status` of the relaunched main process). With the switch off nothing is checked after a
  restart; the tray's *Check for updates…* still works.
- **Notifications**: enable them in Sharkord → Settings → Notifications; a message from another
  account while Sharkord's window isn't focused shows a native notification, in any channel,
  flashes the taskbar until focused (X11; Wayland ignores it), and clicking it brings the window
  back (also from the tray) and opens that channel or DM — test from the channel list and from the
  DM list. On Windows (installed build) the toast is labelled Sharkord. With *Minimize to tray* on,
  leave a text channel open, close the window (`[window] close: hidden to the tray`, `[page]
  [window] page visibility: hidden`) and send a message into that channel: it notifies (`[page]
  [notify] shown: … hidden true`, `[notify] shown: window hidden, unfocused → flash true`, `[page]
  [notify] show`; on Windows `… | by main` and `[notify] show` from main, then `[notify] close:
  timedOut` and the toast is gone from the notification centre); the same minimized (on Wayland: no `[window] minimized` line, the blur is what
  hides it) and with another app focused; with Sharkord focused, none, on that channel or another
  (`[notify] skipped: the window is focused` for another). A notification puts a red dot on the
  tray icon (`[tray] badge on`; on Windows on the taskbar button too), gone once the window is
  focused (`[tray] badge off`). Clicking it logs `[notify] click: channel, item found, …` and
  `[window] show (notification)`; no line names the channel, the author or the message. On Windows
  with Sharkord switched off in Settings → System → Notifications: no toast, `[notify] error: …
  Settings prevent the notification from being delivered`, and the diagnostics file's State says
  `failed 1` and `windows notifications: all on, Sharkord off`.
- **Unreachable page**: stop the server → Retry and Change server both work. While connected, the
  log has `[page] [ws] connecting #1` / `open #1`; the network off gives `[ws] offline`, a `close #1:
  code 1006, unclean` and `connecting #2`... (one per reconnect attempt), back on `[ws] online`.
- **Log files**: launch from the menu (no terminal): `logs/main.log` in the settings folder has the
  `[sys]`, `[flags]`, probe and `[gpu]` lines; a restart moves it to `main.old.log`; a second launch while
  running leaves both untouched; a flood of helper output never leaves more than the two files,
  each <= 5 MB. Nothing in it names the user: `[log]` names the run's date and no path, the server down gives
  `[check] unreachable:` and `[load] failed: server page` without the URL, and an error with a
  path (rename `settings.json` to a directory, change a setting) reads `<userData>/settings.json`;
  on Linux a per-app share audio link logs `[venmic] link 2 app(s)`. A broken `settings.json` (`{`) logs
  `[settings] unreadable:` once; a setting changed in the tab or the tray logs `[settings] <key>: false
  → true (tab|tray)`; `[tray] created`, the second `[sys]` line ends `tray yes`; a second launch logs
  `[app] second launch` and `[window] show (second instance)`; a `throw` in DevTools' console of the
  page (`setTimeout(() => { throw new Error('x') })`) logs `[page] error: uncaught Error: x`.
- **Diagnostics**: Settings → Desktop Client → the Diagnostics card describes the file; **Save
  diagnostics…** opens a save dialog in Downloads with `sharkord-diagnostics-<date>-<time>.txt`; Cancel
  leaves the note empty, Save shows `Saved to …`; the save bar never appears, the switches stay,
  and leaving the tab asks nothing; a double click opens one dialog. The file has the header, the
  sections in order (System, Chromium flags, GPU, Native share, Updates, State, Settings, main.log,
  main.old.log) and both logs; State's `page:` line has the notification permission, `hidden` and
  the four switches, `notifications this run:` the counts, and on Windows `windows notifications:`; `grep` it for the server's host, `$HOME`, the settings folder and
  the user name: no hits, only `<server>`, `<home>`, `<userData>`, `<user>`; the logs on disk
  are unchanged. With `logs/` deleted while running the sections say `(not available: ENOENT)`
  and the save still works. On Windows a JSON path in the probe reads `<home>` (the `\\` form).
- **Theme**: switch Sharkord to light, restart with the server down — local pages and the
  picker should be light too.
- **Screen share picker**: on X11 (source grid) and Wayland (portal, then audio step); cancel
  at each step denies the share cleanly.
- **Share audio (Linux)**: per-app and Entire System; the other side must not hear their own
  voice echoed; audio unlinks when the share stops or the picker is closed.
- **Share audio (Windows)**: "Stream With Audio" loopback.
- **Native screen share (Windows; AMD, NVIDIA, Intel)**: the probe (`[native-share] probe` in the
  log) names the `api` and `device`. With AMF it is on by default (no `nativeShare` in
  `settings.json`), NVENC and Quick Sync need it turned on in Settings → Desktop Client (or the
  tray); turning it off sticks. `SHARKORD_TEST_FAIL=300`: the share ends within ~2 s, the viewer
  isn't left black, the next share is Chromium's. Share a
  screen with H.264, then AV1, simulcast off: a viewer gets 60 fps, rejoining shows a picture
  within a second, and Sharkord's stats show `GPU (Native: AMF, h264_amf)`. A UAC prompt or a
  resolution change mid-share: the picture freezes briefly and comes back. A slow upload (admin
  PowerShell: `New-NetQosPolicy -Name cap -AppPathNameMatchCondition electron.exe
  -ThrottleRateActionBitsPerSecond 3000000 -PolicyStore ActiveStore`, gone at reboot or with
  `Remove-NetQosPolicy`), mid-share and from the start: the viewer stays around 50-60 fps, the
  `sent` lines' pacer mostly under 250 ms, the helper's `held` above 0; lifted, full rate within ~40 s. Two monitors: each one;
  a portrait monitor arrives upright (`input` shows `rotate`, `started` the upright size). The `[native-share] sent …` lines (DevTools console, and the app's log,
  `logs/main.log`, as `[page] [native-share] …`) should keep `lost`/`resync` near zero
  (`pli`/`fir`: keyframe requests from viewers or mediasoup; `keyreq`: helper keyframes asked for;
  `native keys [<KB> rate|req|other]`: the helper's keyframes and why), and an `estimate fell` line
  marks a collapse of Chromium's estimate with the keyframe before it. `resynced after <why>: <ms>,
  <n> frames not sent` reports each loss of the frame swap's sync (a Chromium keyframe for a request,
  frames Chromium dropped) and how long it took to get back.
  While watching, `[native-share] incoming <codec> <size>: decoder …, GPU|CPU` names the decoder of
  each incoming video (once, and again if it changes); Chromium only names it while the page
  captures (the mic in a voice channel). Every 10 s `[native-share] watching …` gives what the viewer
  got: fps, dropped, freezes, jitter buffer, keys, pli, lost, nack, kbps. A window share goes
  native too (`input` shows `"window":true`, `started` its size with `"monitor":"<window>"`, never
  the title); resizing it changes
  the viewer's picture size within ~0.5 s (`capture: window resized`), minimizing keeps the last
  picture, stopping while minimized ends the helper by itself (`exited (0)`), closing it ends the
  share. VP8, or the setting off, must behave exactly as before.
  The `[flags] GPUs:` line lists the adapters; with an AMD GPU `ExpandMediaFoundationEncodingResolutions`
  is disabled, without one it is not (a 4K Chromium-path share then encodes on the GPU).
- **Native screen share (Linux Wayland; AMD, Intel)**: the switch is usable only when the startup
  probe passes (`[native-share] probe` in the log, with `api` and `driver`); otherwise it's greyed
  out with the reason, and "This GPU can hardware encode" lists H.264 / AV1. Share with H.264:
  **one** portal dialog then the audio step, the local preview moves, a viewer gets 60 fps, stats
  show `GPU (Native: Vulkan (hardware), h264_vulkan)`, and the `sent` lines show `native keys` only
  on request (`req`) and none after a bitrate change. With AV1 the local preview moves too (no
  `preview:` errors in the log). With `SHARKORD_ENCODE_API=vaapi` the share is `h264_vaapi` at 60 fps. A still screen shows the helper's `padding`
  near the rate and the estimate staying up; motion after it is at full rate at once, with the
  pacer under 250 ms. At 4K a still screen then motion: no storm of
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
