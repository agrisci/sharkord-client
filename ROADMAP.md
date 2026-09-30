# Roadmap

What would make the client better, most important first. It is built for anyone running a
Sharkord server, on any mix of GPUs, operating systems and networks, so the top of the list is
about never leaving a share broken and being able to debug machines we have never seen.

**Priority:** P0 = before the native share leaves "experimental" · P1 = next · P2 = worthwhile ·
P3 = ideas
**Status:** ✅ Done · 🚧 In progress · 🧪 Needs testing · 📋 Planned · 💡 Idea · ⛔ Won't do
**#** is a stable ID for referring to an item in commits and issues, not its rank; new items take
the next free number (currently 100).

## Bugs

Found while reviewing the code; each is small and should be fixed before new features.

| # | Pri | Item | Status | Notes |
|---|-----|------|--------|-------|
| 76 | P2 | Frame buffering copies large frames repeatedly | 📋 | `Buffer.concat` on every stdout chunk plus a copy per frame (`spawnHelper` in `main.js`); a 4K keyframe is re-copied many times. Keep a chunk list. |
| 77 | P2 | `--check` doesn't prove encoding works | ✅ | It now builds each codec's encoder (every per-device factory) and takes it to READY, reported as `h264`/`av1`; the app runs it at startup on both platforms (#9). |
| 99 | P1 | HiDPI shares encoded at the logical size (Linux) | ✅ | The portal reports a scaled screen's size in logical pixels (a 4K panel at 160%: 2400x1350) while PipeWire delivers physical frames, so the helper downscaled them and the page capped the bitrate for the smaller size (9.7 instead of ~24.9 Mbps). The output size now comes from the source's first caps, and `started` is sent with the first frame, with the sizes negotiated. |
| 86 | P3 | Baseline without the constraint flag | 📋 | With the profile pinned, AMF emits profile_idc 66 (Baseline) but not constraint_set1, and level 5.1: `420433` where the SDP says `42e01f`. Decoders accept it (no Baseline-only tools are used); patch the SPS flag byte or leave it. The helper logs it as a `stream` event. |

## Upstream (Sharkord, mediasoup)

Things best fixed in Sharkord itself; the client can only work around them.

| # | Pri | Item | Status | Notes |
|---|-----|------|--------|-------|
| 28 | P1 | Upgrade mediasoup to ≥ 3.27 | 💡 | Sharkord pins mediasoup 3.19.19 (`apps/server/package.json`), whose transport-cc feedback carries arrival times in whole milliseconds. That matches the 15-50% estimate dips we measured on a clean LAN. Fixed in [mediasoup 3.27.0](https://github.com/versatica/mediasoup/blob/v3/CHANGELOG.md) (PRs #1914, #1917). Would also let #21 be simplified. |
| 79 | P1 | H.264 High profile for screen share | 💡 | The server already offers High `640032` next to Constrained Baseline (`apps/server/src/runtimes/voice.ts:63-73`); the share uses the first H.264 entry. High (CABAC, 8x8 transform) gives noticeably better quality per bit. Needs a Sharkord option or picking it in our hook, plus #72 set to `high`. |
| 32 | P3 | "Bundled payload type collision" on viewers | 💡 | Logged when Sharkord renegotiates the receive connection, with "Inconsistent congestion control feedback types, ignoring all". May weaken the server-to-viewer rate control; report upstream if confirmed. |
| 31 | P2 | Stale-stream keyframe requests | 📋 | A viewer requested keyframes 5x/s for a stream that no longer existed, after earlier shares. Find out whether Sharkord or the client keeps the consumer alive. |

## Chromium path (no helper): flags and fallbacks

Windows window shares, VP8/VP9/auto and simulcast, Linux on X11, the native share turned off or
failed, and every GPU the helper doesn't support go through Chromium's own capture and encoders. On Windows we change four Chromium
features for that path (Linux: see below the table) (`electron/main.js`, Chromium flags section). Checked against Chromium 152:
all are experimental, none can be scoped per vendor or codec (no feature parameters), and a future
Electron can rename or drop them without any error.

| Flag we set | Chromium 152 default | Why we set it | Risk | Criticality |
|---|---|---|---|---|
| `disable-features=ExpandMediaFoundationEncodingResolutions` | **On** since ~M130 (`media/gpu/windows/mf_video_encoder_switches.cc`) | With it on, Chromium drops AMD's AV1 encoder (likely because it probes at 1080 lines and AMD AV1 needs 64x16 alignment; inferred, no Chromium bug, unfixed on `main`) | Off caps **every** hardware encoder, all vendors and codecs, at 1920x1080 (`mf_video_encoder_util.h`, `media_foundation_video_encode_accelerator_win.cc`, no vendor check). NVIDIA/Intel/AMD shares above 1080p on this path are encoded on the CPU, worse than stock Chrome. Adapting resolution across 1080p also switches encoders mid-call, where we saw the hardware encoder stall. | **Critical** |
| `enable-features=WebRtcAllowWgcUsingTexture` | Off; **new in M151** (CL 7660639, June 2026) | Capture as GPU textures: 4K at ~58 fps instead of ~36 | If the GPU adapter changes (hybrid laptops, a GPU process restart) capture fails for good (`kDesktopCaptureDeviceGpuAdapterChanged`, no readback fallback yet). HDR colour space unset. The newest code of the four. | **High** on multi-GPU machines, low otherwise |
| `enable-features=PlatformH264CbpEncoding` | Off on Windows ("rolled out later with corresponding ISV") | Hardware for the Constrained Baseline H.264 (`42e01f`) Sharkord negotiates | Encoder errors fall back to software (`RTCVideoEncoder::NotifyErrorStatus`); a driver producing a bad stream wouldn't. **NVIDIA is always skipped for Constrained Baseline** (crbug 1088650), so NVIDIA H.264 on this path is CPU regardless. | Medium-low |
| `enable-features=WebRtcAV1HWEncode` | Off on Windows | Hardware AV1 | Chromium's vendor blocklists still apply (Intel's hybrid AV1 encoder is skipped); AV1 drivers are the youngest. | Medium-low |

On Linux (checked against Chromium 152's source, on a Cezanne iGPU with an RTX 3050 on nouveau,
KDE Wayland) we used to set seven switches; one is left, and only with *Hardware encoding for other
shares* on (off by default):

| Linux switch | Chromium 152 default | Now | Why |
|---|---|---|---|
| `enable-features=AcceleratedVideoEncoder` | Off | **Opt-in** | VA-API encoding for Chromium's path. A driver can open the encoder and still produce a stream viewers can't play (infinite loading or garbled, Vesktop #1004 on Mesa 24.3 / RX 6600), with no fallback; Vesktop ships it off too. The helper checks its own encoder at startup. |
| `AcceleratedVideoDecodeLinuxGL`, `AcceleratedVideoDecodeLinuxZeroCopyGL` | On | Dropped | Already the default. |
| `VaapiIgnoreDriverChecks` | Off | Dropped | Only read when Chromium renders with Vulkan, which Wayland doesn't allow (GL through ANGLE instead). |
| `ignore-gpu-blocklist` | -- | Dropped | Changed nothing on current Mesa AMD/Intel; overrode every entry for broken setups (nouveau, software GL, VMs, old drivers) and turned on WebGPU through Vulkan (the "not compatible with Vulkan" error at startup). |
| `enable-gpu-rasterization`, `enable-zero-copy` | On / off | Dropped | Page drawing only; forcing them bypassed the blocklist. |

| # | Pri | Item | Status | Notes |
|---|-----|------|--------|-------|
| 88 | P0 | Stop capping every GPU at 1080p | 📋 | Keep Chromium's default everywhere except where it hurts: disable `ExpandMediaFoundationEncodingResolutions` only when an AMD GPU is present. Detect the vendor before `ready` (Electron has no GPU info yet): `%SystemRoot%\System32\reg.exe query HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318} /s /v MatchingDeviceId`, match `PCI\VEN_1002` (AMD), `10DE` (NVIDIA), `8086` (Intel); ~70 ms, `windowsHide`, 2 s timeout, try/catch → today's behaviour on failure. Only `PCI\` IDs count (skips virtual adapters); log the result. `wmic` is gone on 24H2/25H2 and PowerShell CIM takes ~1.3 s. AMD whole-screen shares use the helper, so AMD only keeps the cap for window shares and with the helper off. Covers #54. |
| 89 | P0 | One merged, logged feature list + kill switch | ✅ | Like Vesktop: the command line's `enable-features`/`disable-features` are merged with ours (a disabled one wins), appended once and logged (`[flags]`). The setting *Hardware encoding for other shares* (`chromiumHwEncode`, next launch; on on Windows, off on Linux) skips all our flags, and `SHARKORD_CHROMIUM_DEFAULTS=1` forces it off for support. |
| 90 | P1 | Texture capture only where it's safe | 📋 | Enable `WebRtcAllowWgcUsingTexture` only with a single hardware GPU (count the `PCI\VEN_` adapters from #88's probe); off on hybrid and multi-GPU machines, where an adapter change breaks capture for good. Revisit when Chromium adds its readback fallback. |
| 91 | P1 | Re-verify the flags on every Electron upgrade | 📋 | Release-checklist step: `chrome://gpu`, the sender's `encoderImplementation`, and 4K capture fps on the Chromium path. Log the applied features and, per share, the encoder Chromium really used (feeds #34). |
| 92 | P1 | NVIDIA H.264 on the Chromium path | 📋 | Chromium never uses NVIDIA's hardware encoder for Constrained Baseline, which is what Sharkord negotiates; High profile works. Fixed by #79 / #48; until then NVIDIA shares H.264 on the CPU (AV1 on RTX 40+ is hardware). |
| 93 | P2 | "Prefer smoothness / clarity" for Chromium-path shares | 💡 | Sharkord hard-sets `contentHint = 'detail'`, which keeps resolution at the cost of frame rate and made the bandwidth estimate sawtooth in our tests. Vesktop defaults to `motion` and lets the user choose ("Prefer Smoothness" / "Prefer Clarity"). A desktop option applied in our `getDisplayMedia` hook; default: Sharkord's behaviour. |
| 94 | P2 | Hardware acceleration escape hatch | 💡 | A Vesktop-style "Hardware acceleration" toggle (`app.disableHardwareAcceleration()`, restart) for broken drivers, and support notes for Chromium's per-codec GPU workaround switches (`--disable_accelerated_h264_encode`, `--disable_accelerated_av1_encode`). |
| 95 | P3 | Small Chromium defaults from Vesktop | 💡 | Evaluate `autoplay-policy=no-user-gesture-required` (Sharkord's sounds before any click) and disabling `HardwareMediaKeyHandling` / `MediaSessionService` (media keys); only if a real problem shows up. |

## Native screen share: robustness

| # | Pri | Item | Status | Notes |
|---|-----|------|--------|-------|
| 2 | P0 | Runtime self-check of the frame swap | 📋 | The swap relies on Chromium behaviour (a size change keys that frame, timestamp smoothing). Watching pairing health lets an Electron update that changes this fall back instead of breaking shares. |
| 3 | P0 | Clean fallback on capture loss | 🚧 | Helper errors, exits and stalls now fall back (#1, #70, #71); a suspend (Linux, KDE: the portal's stream doesn't come back on resume) ends the share through the watchdog without turning the helper off for the session, and a resolution change with the same aspect ratio keeps the share running (the output size stays). A change of aspect ratio keeps running but stretches (#98). Still to test: secure desktop / UAC, monitor unplugged, driver reset, which end the pipeline (`main.rs` pump) and should arrive as an `error` event. |
| 85 | P2 | Continue a failed share in place | 💡 | Today a mid-share helper failure ends the share (the user shares again, on Chromium's path). Continuing in place failed three ways: swapping the capture track in restarts the RTP timestamps from the capture's older clock (viewers drop every frame as stale; re-stamped frames keep the capture metadata), Chromium's hardware encoder switched in mid-share stalls after a few frames (<= 1080p), and 4K in software runs at 3-11 fps. A way through: stamp the placeholder on the capture's clock from the start, so a later swap stays continuous. #88 removes the 1080p hardware boundary on NVIDIA/Intel that made mid-share encoder switches likely. |
| 87 | P1 | Test the early fallback (helper never starting, Windows) | 🧪 | VP8/VP9, auto and simulcast no longer reach the helper (#96). Left: a helper that fails before its first frame; the connection gets the capture track swapped in after the placeholder's black frames went out, so the same timestamp jump as #85 could freeze viewers. Test with the setting on and no AMD GPU. |
| 4 | P1 | Test the installed build | 🧪 | The rate-control work ran on the dev build; run a 4K share on a `dist:win` build (packaged GStreamer subset, clean environment). |
| 5 | P1 | Viewer joining mid-share | 🧪 | Time to first frame for a late or reconnecting viewer (keyframe request → native keyframe → aligned Chromium keyframe). |
| 6 | P2 | Stress the pairing | 💡 | Hours-long shares, sleep/resume, CPU/GPU saturation, many viewers. |

## Native screen share: hardware coverage

| # | Pri | Item | Status | Notes |
|---|-----|------|--------|-------|
| 7 | P0 | NVIDIA encoder (NVENC) | 📋 | On Windows only AMD AMF is tried (`encoder()` in `main.rs`; Linux uses VA-API). GStreamer 1.28's Windows build ships `nvcodec`; `nvh264enc` changes bitrate with NVENC `Reconfigure()`, no new keyframe. |
| 8 | P0 | Intel encoder (Quick Sync) | 📋 | `qsvh264enc` / `qsvav1enc` (shipped in 1.28). Bitrate changes via `Reset` without a new sequence (briefly drains queued frames). |
| 9 | P0 | Startup capability probe | 🚧 | `--check` opens each codec's encoder (#77) and the app runs it once at startup on both platforms: the switch is usable only when an encoder opens (otherwise greyed out, with the reason), the tab lists what it found, and a share goes native only with a codec it opened. Left: turn the setting on by default when it passes. |
| 10 | P1 | Media Foundation fallback | 📋 | `mfh264enc` (shipped in 1.28) as a vendor-neutral H.264 fallback. |
| 11 | P1 | Hybrid-GPU laptops | 📋 | Capture on the iGPU, encode on the dGPU: choose the adapter per monitor, avoid slow cross-adapter copies. |
| 12 | P1 | Monitor edge cases | 📋 | HDR (tone-map or refuse), >60 Hz panels, portrait, mixed DPI, hotplug and resize mid-share (a new aspect ratio stretches, #98). The output size is fitted once, to the source's first caps (a probe in `build()` in `main.rs`; the portal's own size is in logical pixels, #99). |
| 98 | P2 | Aspect ratio change mid-share (Linux) | 📋 | Switching a 16:9 panel to 5:4 (1280x1024) mid-share keeps the share running but stretches it 1.42x: the output size is fixed at start and the new frames are scaled into it (reproduced offline: a square came out 768 px wide instead of 540). Borders don't work: KWin's stream says `pixel-aspect-ratio=1/2147483647` (pinning 1/1 at the encoder overflows `vapostproc`'s size calculation and the share fails at once), and with the ratio corrected `vapostproc add-borders` leaves the border area unpainted on Mesa radeonsi (green, black or the previous frames' content, whatever the GPU memory held). Fix: on a source caps change, re-fit the output size (`fit()`) and update the encoder caps; the encoder restarts with a keyframe, which WebRTC viewers handle. Rare in practice (changing the screen's shape during a share). |
| 14 | P2 | Honour Sharkord's cursor setting | 📋 | Sharkord offers Always / While moving / Never (`screenCursor`, passed as `getDisplayMedia` `video.cursor`); the helper always draws it (`show-cursor=true` in `source()` on Windows, the portal's embedded cursor in `portal.rs` on Linux). |
| 13 | P2 | Window capture | 💡 | On Windows only screens go native (`nativeTargetFor` in `main.js`); `d3d11screencapturesrc` can capture a window via `window-handle`. Linux already shares windows through the portal. |
| 15 | P3 | Share audio in the helper | 💡 | Audio stays on Chromium's loopback; only worth moving if sync or quality issues appear. |

## Codecs

| # | Pri | Item | Status | Notes |
|---|-----|------|--------|-------|
| 47 | ✅ | H.264 and AV1 on the native path | ✅ | AMD AMF; AV1 keyframes forced with a GOP poke. |
| 48 | P1 | H.264 High profile | 📋 | Client side of #79: pin the profile (#72), then encode High when the server's High entry is negotiated. |
| 49 | P1 | AV1 on NVIDIA and Intel | 📋 | Follows #7/#8: `nvav1enc` (RTX 40+), `qsvav1enc` (Arc, newer iGPUs); they register only where the GPU supports AV1. |
| 50 | P2 | VP9 on the native path | 💡 | The server offers VP9 profile 0; Intel encodes VP9 (`qsvvp9enc`) on GPUs without AV1. VP8/VP9 fall back to Chromium today. |
| 52 | P2 | Suggest the codec from the probe | 💡 | Propose the best codec the sender's GPU encodes and viewers can decode, instead of a fixed setting. |
| 53 | P3 | 10-bit / HDR AV1 | 💡 | With #12; needs HDR-capable viewers. |
| 54 | P3 | Chromium path: hardware AV1 above 1080p | 💡 | The current flags cap hardware encoders at 1080p so AMD's AV1 encoder is accepted. Replaced by #88 (cap only on AMD); what remains is AMD itself above 1080p. |
| 51 | ⛔ | H.265 / HEVC | ⛔ | mediasoup removed H.265 in 3.16.6 and the server doesn't offer it, even though Chromium's WebRTC supports it in hardware. |

## Rate control and quality

| # | Pri | Item | Status | Notes |
|---|-----|------|--------|-------|
| 23 | P1 | Test on real internet uploads | 🧪 | Only tested on a LAN. Try a slow and a bufferbloated home upload: does the loss / round-trip check back off early enough? |
| 24 | P1 | Faster start | 📋 | Reaching full rate takes ~30 s from Chromium's ~5 Mbps start. Start from the last good rate per server (still guarded by the congestion check). |
| 25 | P2 | Fewer keyframes on bitrate changes | 💡 | GStreamer's AMF element re-initialises the encoder on any property change (`property_updated`), a 4K keyframe per step; `vah264enc` does the same (`bitrate` sets `reconf`, the next frame drains and reopens the encoder, in 1.28 and `main`). #27 now keeps keyframes from lowering the bitrate. Use fewer, larger steps. NVENC/QSV (#7, #8) don't have this. |
| 26 | P2 | Slider vs resolution cap | 💡 | Decide whether Sharkord's slider alone should limit; today 4K60 stops at ~25 Mbps (0.05 bits per pixel per frame) even with a higher slider. |

## Viewers

| # | Pri | Item | Status | Notes |
|---|-----|------|--------|-------|
| 29 | P1 | Weak decoders | 📋 | Everyone gets the same stream; a Ryzen 4000 iGPU fell behind at 4K60 25 Mbps H.264. Suggest a lower resolution when the slider is high, and document which GPUs decode what (#66). |
| 30 | P2 | Simulcast for native shares | 💡 | The helper encodes two layers (e.g. 4K + 1080p) and mediasoup picks per viewer. Big change; only once people hit #29. |

## Linux

| # | Pri | Item | Status | Notes |
|---|-----|------|--------|-------|
| 16 | P1 | Measure sharing from Linux | 🧪 | The native share is measured (1080p60 H.264, #17). Chromium's own path still to measure at 1080p/1440p/4K, H.264 and AV1 (Wayland portal, VA-API, venmic). |
| 17 | P2 | Native share on Linux | 🧪 | Wayland + VA-API (`vah264enc`, `vaav1enc`), system GStreamer. The helper owns the only portal dialog; the page decodes its frames for the preview. 60 fps 1080p H.264 on a Renoir iGPU, and from the installed rpm (CI build) on a Cezanne iGPU under KDE, portal and audio-step cancels included. To test: AV1 (RDNA3+), Intel iHD, GNOME/mutter, 4K, the AppImage and deb. |
| 96 | P2 | Native share: codec known only from Sharkord's settings | 📋 | The helper is used only when H.264/AV1 is picked with Simulcast off, read from Sharkord's localStorage before the share (on Linux the helper must pick before the codec is negotiated), and the server's `webRtcSimulcastEnabled` from Sharkord's WebSocket messages (simulcast turns the codec into VP8). A renamed key silently turns the helper off; `auto` never uses it. A signal from the server or the SDP would remove the dependency. |
| 18 | P2 | NVIDIA and X11 on Linux | 💡 | No VA-API encode on NVIDIA (NVENC instead); X11 sessions use the source grid. |
| 97 | P3 | Picture-in-picture window shows the Wayland icon | 📋 | Chromium's video PiP window (`VideoOverlayWindowViews`) sets no Wayland app ID (no `WM_CLASS` on X11), so KWin can't match it to `sharkord.desktop`: it borrows the main window's icon while that is shown and falls back to the generic Wayland icon once it hides to the tray. Upstream: [brave-browser#57390](https://github.com/brave/brave-browser/issues/57390), fixed in Brave by patching Chromium ([brave-core#38210](https://github.com/brave/brave-core/pull/38210)); needs the same in Electron. Workaround if it matters: our own PiP window behind Sharkord's `requestPictureInPicture`. |
| 19 | P3 | macOS | 💡 | Not built (`package.json` has no mac target). ScreenCaptureKit + VideoToolbox if there is demand. |

## Diagnostics and support

| # | Pri | Item | Status | Notes |
|---|-----|------|--------|-------|
| 33 | P0 | Always-on log files | ✅ | `log()` also writes `userData/logs/main.log` on every launch (menu, tray, login), with the previous run in `main.old.log`: two files of at most 5 MB, rotated at startup and when full (checked with a `GST_DEBUG=9` flood). The helper's stderr goes in as `[helper]` lines; the startup probe streams it too (`execFile` killed it past 1 MB). |
| 34 | P0 | "Copy diagnostics" button | 📋 | Settings → Desktop Client: GPU and encoder list (#9), Electron/Chromium version, OS, last share stats, recent log. Turns "it's black for me" into a fixable report. |
| 35 | P1 | Quieter share logging | 📋 | One summary line every ~10 s by default, per-2 s detail behind a debug switch; drop the worker's debug counters (`ckNoNk`, `nkNoCk`). |
| 36 | P2 | Hardware test matrix | 💡 | A checklist for volunteers (GPU vendor × codec × resolution × OS) plus the diagnostics output, collected in an issue. |

## CI and releases

| # | Pri | Item | Status | Notes |
|---|-----|------|--------|-------|
| 37 | ✅ | First green CI run with the helper | ✅ | Both jobs build and stage the helper with `SHARKORD_REQUIRE_NATIVE=1`: Windows with its GStreamer bundle and `--check`, Linux (Ubuntu 22.04) against the system's GStreamer. |
| 55 | P1 | Cache GStreamer and Cargo | 📋 | Only npm is cached (`build.yml`); GStreamer is downloaded and the helper rebuilt every run. Cache the installer, `~/.cargo` and `native/target`. |
| 56 | P1 | Lint and format checks | 📋 | ESLint (repo style: no semicolons) for `electron/`, `cargo fmt --check` + `cargo clippy` for `native/`. There are no lint or test scripts today. |
| 38 | P1 | Tests for the pairing logic | 📋 | The worker's frame pairing is the most fragile code and needs no GPU: feed it synthetic frames (drops, timestamp drift, keyframes, PLIs) in CI. |
| 58 | P1 | Installer smoke test | 📋 | Install the NSIS build on the runner, launch it, assert a local page loads and the app quits cleanly. |
| 39 | P1 | Code signing | 📋 | Unsigned installer plus a spawned `.exe` trips SmartScreen and antivirus tools (README already warns about SmartScreen). |
| 42 | P2 | Auto-update | 💡 | electron-updater with GitHub Releases works for NSIS and AppImage (not Flatpak). |
| 59 | P2 | Checksums and provenance | 💡 | SHA-256 sums and GitHub artifact attestations with each Release. |
| 60 | P2 | ARM64 builds | 💡 | electron-builder handles NSIS and AppImage arm64; the native helper would need an ARM encoder. |
| 41 | P2 | Licence notices | 📋 | GStreamer licence texts already ship per DLL (`LICENSES` in `scripts/stage-native.js`). Missing: a top-level third-party notice, an About entry, the VC++ runtime terms. |
| 40 | P2 | Helper size | 💡 | Check the staged plugin set (`PLUGINS` in `stage-native.js`) per encoder once #7/#8 add plugins. |
| 61 | P3 | Flatpak / AUR | 💡 | electron-builder's Flatpak target makes single-file bundles only, with no auto-update; Flathub would need its own manifest. |
| 62 | P3 | Nightly builds from `dev` | 💡 | Artifacts exist per push; a pinned pre-release is easier for testers to find. |

## Client (general)

| # | Pri | Item | Status | Notes |
|---|-----|------|--------|-------|
| 63 | P2 | Crash handling | 💡 | Nothing handles `render-process-gone` / `child-process-gone`: reload the page, stop the helper, optional local crash dumps. |
| 44 | P2 | Several servers | 💡 | Only one `serverUrl` is saved; switch between saved servers. |
| 45 | P2 | Push-to-talk / global shortcuts | 💡 | OS-level shortcuts a browser can't do (`globalShortcut`, Wayland needs the GlobalShortcuts portal). |
| 64 | P2 | Translations for injected UI | 💡 | "Change server" and the desktop options are English only (`preload.js`); follow Sharkord's language setting. |
| 46 | P3 | Picker: live previews, remember last source | 💡 | Static thumbnails exist; live ones and remembering the last pick don't. |
| 65 | P3 | Accessibility of injected controls | 💡 | Switches have `role="switch"` / `aria-checked`; their labels aren't associated with them. |

## Documentation

| # | Pri | Item | Status | Notes |
|---|-----|------|--------|-------|
| 66 | P1 | Supported hardware page | 📋 | What each GPU vendor gets (native vs Chromium path, codecs, resolutions) and what viewers need to decode it. |
| 67 | P1 | Troubleshooting guide | 📋 | Black screen, low bitrate, stutter: what to check and how to attach diagnostics (#34). |
| 68 | P2 | Contributor guide | 💡 | Building the helper, running with logging, testing with two accounts (`AGENTS.md` covers most of it for agents). |

## Done

| # | Item | Notes |
|---|------|-------|
| 20 | Bitrate from Chromium's transport estimate | Not `targetBitrate`, which the transform's extra bytes halve (libwebrtc post-encode overhead). |
| 21 | Hold through false estimate dips | Step up with the estimate while loss and round trip are clean, never down; 0.85x on real congestion. |
| 22 | Cap from Sharkord's bitrate slider | Plus 0.05 bits per pixel per frame (~25 Mbps at 4K60). |
| 43 | Tray Quit during a call | Sharkord's leave guard silently blocked closing; Quit always wins, X asks first. |
| 81 | In-memory placeholder frames | Canvas frames needed a GPU readback and stalled the share at 15 fps under GPU load. |
| 82 | No screen-content hint on the placeholder | Screen-content probing knocked Chromium's estimate down every 5 s. |
| 83 | Fixed resolution | The share keeps the resolution the user picked instead of following bandwidth. |
| 84 | Hardware H.264/AV1 on the Chromium path (Windows) | `PlatformH264CbpEncoding`, `WebRtcAllowWgcUsingTexture`, `WebRtcAV1HWEncode`. |
| 1 | Stall watchdog | No helper frame for 10 s, or none swapped for 6 s: before the first frame on Windows the share switches to Chromium's capture, otherwise it ends. |
| 27 | Pacer delay as a congestion signal | Over 250 ms counts as congestion: after a quiet stretch the estimate fell 32 → 5.6 Mbps and holding the rate queued 0.8 s of lag. A keyframe's queue (50-180 ms, over two readings on Wi-Fi) no longer does: it lowered the bitrate, the change restarted the encoder with another keyframe, and a 1080p share keyed every 5-10 s on a clean cable, every ~17 s on Wi-Fi with an intermediate rule (over 50 ms twice running). Replayed on the logged shares: 42 false alarms → 0; loss and round trip still flag the Wi-Fi storm. Same threshold `sharkord-native-client` measured (250 ms). |
| 69 | EPIPE on the helper's stdin | `proc.stdin` has an `error` listener. |
| 70 | Helper start failure reported | Every failure after `start` sends an `error` event; `started` is sent once capture runs. |
| 71 | Helper crash ends the share cleanly | Main reports an unexpected exit to the page; mid-share the share ends like a stopped capture and the session falls back to Chromium's own path (verified by ending the helper mid-share). |
| 72 | H.264 profile pinned | `profile=constrained-baseline` in the helper's caps; see #86 for the remaining flag. |
| 73 | Helper port routed per share | Ports carry the share's id; verified with five quick restarts. |
| 74 | Atomic settings writes | Temp file + rename; failures are logged. |
| 75 | First-launch parse guarded | A malformed form message is ignored. |
| 78 | Dead resize path and stale comments removed | `size`/`resized`, the "4 s keyframe" and "Alt menu" comments. |
| 80 | README brought up to date | Native share, Windows hardware encoding, helper prerequisites, CI, structure, and where the project is going. |
