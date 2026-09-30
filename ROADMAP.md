# Roadmap

What would make the client better, most important first. It is built for anyone running a
Sharkord server, on any mix of GPUs, operating systems and networks, so the top of the list is
about never leaving a share broken and being able to debug machines we have never seen.

**Priority:** P0 = before the native share leaves "experimental" · P1 = next · P2 = worthwhile ·
P3 = ideas
**Status:** ✅ Done · 🚧 In progress · 🧪 Needs testing · 📋 Planned · 💡 Idea · ⛔ Won't do
**#** is a stable ID for referring to an item in commits and issues, not its rank; new items take
the next free number (currently 106).

## Bugs

Found while reviewing the code; each is small and should be fixed before new features.

| # | Pri | Item | Status | Notes |
|---|-----|------|--------|-------|
| 76 | P2 | Frame buffering copies large frames repeatedly | 📋 | `Buffer.concat` on every stdout chunk plus a copy per frame (`spawnHelper` in `main.js`); a 4K keyframe is re-copied many times. Keep a chunk list. |
| 77 | P2 | `--check` doesn't prove encoding works | ✅ | It now builds each codec's encoder (every per-device factory) and takes it to READY, reported as `h264`/`av1`; the app runs it at startup on both platforms (#9). |
| 104 | P0 | Linux helper without GStreamer, working on a clean install | 🧪 | PipeWire (DMA-BUF) → `scale_vulkan` → FFmpeg `h264_vulkan`/`av1_vulkan` (VA-API fallback), our own static LGPL FFmpeg with runtime-bitrate and frame-size-cap patches, an 8-frame VBV (VBR on Vulkan: ~130 kbps on a still screen; CBR on VA-API), keyframes only on request, paced by the helper, zero-copy DMA-BUF capture (4K60 at 6-9% CPU); RADV/ANV with H.264 encode ship in the package for distros that strip it, and rank before the system's VA-API (tested on Fedora's stock Mesa, and on a Renoir laptop, VCN2, where they replaced RPM Fusion's VA-API). Live desktop (RX 9060 XT) → laptop (Renoir, Ethernet), 4K60 H.264 25 Mbps, video with scene cuts and still pauses: 60 fps in every window, 0 freezes, 0 drops, 0 keyframe requests. Laptop (Renoir, VCN2) → desktop on the bundled RADV, 1080p60 zero-copy: 60 fps, 0 freezes; but Renoir's encoder keeps VBR at the target on a still screen (~5 Mbps, like `sharkord-native-client` KI-33 under VA-API), a firmware limit. Left: clean-install VMs, Intel, CI. Sunshine's design (`upstream/Sunshine`). |
| 103 | P1 | Windows helper on FFmpeg | 🧪 | `ddagrab` → `scale_d3d11` → `h264_amf`/`av1_amf` (also `*_nvenc`, `*_qsv`, #7/#8), our static LGPL FFmpeg 9.0 built with MSVC (`scripts/deps/ffmpeg-windows.sh`), one 1.6 MiB exe, no GStreamer. Patches: bitrate changes in place for AMF, NVENC and Quick Sync; `ddagrab` polls DXGI (waiting in `AcquireNextFrame` held the D3D11 lock and starved AMF: 39-53 fps, now 60); `scale_d3d11` BT.709 and a texture per frame. AMF ignores its VBV, HRD and `max_au_size` (RX 9060 XT: a scroll after a still screen came out at 1.2 MB): a minimum QP 18 (AV1: q-index 60) caps it at 297 KB. Measured on the helper alone (4K60 desktop): 60 fps, keyframes 90-130 KB on request only, a bitrate change without a keyframe. Left: a live share to a viewer, UAC / mode change mid-share, two monitors, the installed build (#4). |
| 100 | P1 | 4K shares stutter on Linux (VA rate control) | ✅ | Measured desktop RX 9060 XT → Renoir laptop, 4K60 H.264 at 25 Mbps: `vah264enc` in VBR sent 28-39 Mbps asked for 21 (a lower target or `target-percentage` changes nothing, `cpb-size` is inert), over Chromium's pacer: queue up to 850 ms, viewer freezes ~6 s per 110 s. An interim CBR fix (PR #9) held 20-22 Mbps; the Linux helper now encodes with FFmpeg instead (#104): CBR with an 8-frame VBV, rate on target, VMAF 95.9 on video against 91.8. |
| 101 | P0 | Keyframe storm from the frame swap | 🧪 | On a clean link (Ethernet, 0 loss) a still 4K screen keyed at 1.1-1.8 MB (`vah264enc`), more than Chromium's hardware decoder path absorbs: the viewer asked for a keyframe 4x/s, the placeholder keyed on each request, the swap lost sync for ~570 ms waiting out the 1 s throttle, and the viewer sat at 0-7 fps. Now keyframes are capped (~370-515 KB at 4K, #104) and the Linux helper is asked within 300 ms. To confirm live: still screen then motion at 4K. |
| 102 | P2 | A freeze at every scheduled keyframe (Linux) | ✅ | `vah264enc` keyed every 1024 frames (17 s), each 1.1 MB at 4K and followed by a viewer request and a freeze. The Linux helper now keys only on request (#104). |
| 99 | P1 | HiDPI shares encoded at the logical size (Linux) | ✅ | The portal reports a scaled screen's size in logical pixels (a 4K panel at 160%: 2400x1350) while PipeWire delivers physical frames, so the helper downscaled them and the page capped the bitrate for the smaller size (9.7 instead of ~24.9 Mbps). The output size now comes from the source's first caps, and `started` is sent with the first frame, with the sizes negotiated. |
| 86 | P3 | Baseline without the constraint flag | 🧪 | GStreamer's AMF emitted `420433` (Baseline without constraint_set1) where the SDP says `42e01f`. FFmpeg's AMF (#103) with `profile=constrained_baseline` sends `424033`: the flag is set. To confirm on a live share (`stream` event). |

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
| 3 | P0 | Clean fallback on capture loss | 🚧 | Helper errors, exits and stalls now fall back (#1, #70, #71); a suspend (Linux, KDE: the portal's stream doesn't come back on resume) ends the share through the watchdog without turning the helper off for the session, and a resolution change with the same aspect ratio keeps the share running (the output size stays). A change of aspect ratio keeps running but stretches (#98). Windows (#103) rebuilds a lost desktop duplication for 5 s while the encoder repeats the last picture, then sends `error`. Still to test: secure desktop / UAC, monitor unplugged, driver reset. |
| 85 | P2 | Continue a failed share in place | 💡 | Today a mid-share helper failure ends the share (the user shares again, on Chromium's path). Continuing in place failed three ways: swapping the capture track in restarts the RTP timestamps from the capture's older clock (viewers drop every frame as stale; re-stamped frames keep the capture metadata), Chromium's hardware encoder switched in mid-share stalls after a few frames (<= 1080p), and 4K in software runs at 3-11 fps. A way through: stamp the placeholder on the capture's clock from the start, so a later swap stays continuous. #88 removes the 1080p hardware boundary on NVIDIA/Intel that made mid-share encoder switches likely. |
| 87 | P1 | Test the early fallback (helper never starting, Windows) | 🧪 | VP8/VP9, auto and simulcast no longer reach the helper (#96). Left: a helper that fails before its first frame; the connection gets the capture track swapped in after the placeholder's black frames went out, so the same timestamp jump as #85 could freeze viewers. Test with the setting on and `SHARKORD_ENCODE_API` forcing an API the GPU lacks. |
| 4 | P1 | Test the installed build | 🧪 | Run a 4K share on a `dist:win` build (the static helper from #103, clean environment). |
| 5 | P1 | Viewer joining mid-share | 🧪 | Time to first frame for a late or reconnecting viewer (keyframe request → native keyframe → aligned Chromium keyframe). |
| 6 | P2 | Stress the pairing | 💡 | Hours-long shares, sleep/resume, CPU/GPU saturation, many viewers. |

## Native screen share: hardware coverage

| # | Pri | Item | Status | Notes |
|---|-----|------|--------|-------|
| 7 | P0 | NVIDIA encoder (NVENC) | 🧪 | Built into the Windows helper (#103): `h264_nvenc`/`av1_nvenc` with ultra-low latency, no scene-cut keyframes, and our patch for bitrate changes without a keyframe (NVENC `Reconfigure()` without reset). Untested: needs an NVIDIA GPU (drivers 531+). On Linux FFmpeg's NVENC or NVIDIA's Vulkan video (#18). |
| 8 | P0 | Intel encoder (Quick Sync) | 🧪 | Built into the Windows helper (#103): `h264_qsv`/`av1_qsv` through libvpl, the converted frames mapped into QSV, our patch resetting a rate change without a new sequence, the frame cap as `max_frame_size`. Untested: needs an Intel GPU. On Linux Intel already goes through Vulkan video (ANV) or VA-API (#104). |
| 9 | P0 | Startup capability probe | 🚧 | `--check` opens each codec's encoder (#77) and the app runs it once at startup on both platforms: the switch is usable only when an encoder opens (otherwise greyed out, with the reason), the tab lists what it found, and a share goes native only with a codec it opened. Left: turn the setting on by default when it passes. |
| 10 | P2 | Media Foundation fallback | 💡 | FFmpeg's `h264_mf` as a vendor-neutral H.264 fallback, for GPUs none of AMF, NVENC and Quick Sync opens on. |
| 11 | P1 | Hybrid-GPU laptops | 📋 | Capture on the iGPU, encode on the dGPU: choose the adapter per monitor, avoid slow cross-adapter copies. |
| 12 | P1 | Monitor edge cases | 📋 | HDR (tone-map or refuse), >60 Hz panels, portrait, mixed DPI, hotplug and resize mid-share. Linux sizes the output from the negotiated PipeWire format and re-fits on a change (#98); Windows re-fits when the captured size changes and takes 8-bit BGRA from DXGI on an HDR desktop (#103), both untested live. |
| 98 | P2 | Aspect ratio change mid-share (Linux) | 🧪 | A change of the source's size mid-share now re-fits the output (`fit`) and restarts the encoder with a keyframe (`native/src/linux/mod.rs`), instead of scaling into the old size (which stretched a 16:9 → 5:4 switch 1.42x). To test live. |
| 14 | P2 | Honour Sharkord's cursor setting | 📋 | Sharkord offers Always / While moving / Never (`screenCursor`, passed as `getDisplayMedia` `video.cursor`); the helper always draws it (`show-cursor=true` in `source()` on Windows, the portal's embedded cursor in `portal.rs` on Linux). |
| 13 | P2 | Window capture | 💡 | On Windows only screens go native (`nativeTargetFor` in `main.js`); `d3d11screencapturesrc` can capture a window via `window-handle`. Linux already shares windows through the portal. |
| 15 | P3 | Share audio in the helper | 💡 | Audio stays on Chromium's loopback; only worth moving if sync or quality issues appear. |

## Codecs

| # | Pri | Item | Status | Notes |
|---|-----|------|--------|-------|
| 47 | ✅ | H.264 and AV1 on the native path | ✅ | AMD AMF; AV1 keyframes forced with a GOP poke. |
| 48 | P1 | H.264 High profile | 📋 | Client side of #79: pin the profile (#72), then encode High when the server's High entry is negotiated. |
| 49 | P1 | AV1 on NVIDIA and Intel | 🧪 | Built in with #7/#8: `av1_nvenc` (RTX 40+), `av1_qsv` (Arc, newer iGPUs); the probe reports AV1 only where one encodes. |
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
| 25 | P2 | Fewer keyframes on bitrate changes | ✅ | FFmpeg patched to apply a bitrate change on the next frame without an IDR: Vulkan re-issues only its rate control, VA-API re-sends its rate-control parameter (#104), AMF sets its rate properties, NVENC reconfigures without reset, Quick Sync resets without a new sequence (#103). The page steps 10% every 2 s on both platforms. |
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
| 17 | P2 | Native share on Linux | 🧪 | Wayland; PipeWire → FFmpeg (Vulkan video, else VA-API) since #104, bundled Mesa drivers where the distro strips H.264. The helper owns the only portal dialog; the page decodes its frames for the preview. Measured before #104: 60 fps 1080p H.264 on a Renoir iGPU, and the installed rpm on a Cezanne iGPU. To test with #104: 4K on RDNA4 (live), Intel (ANV/iHD), GNOME/mutter, the AppImage and deb, a clean Fedora and Ubuntu install. |
| 96 | P2 | Native share: codec known only from Sharkord's settings | 📋 | The helper is used only when H.264/AV1 is picked with Simulcast off, read from Sharkord's localStorage before the share (on Linux the helper must pick before the codec is negotiated), and the server's `webRtcSimulcastEnabled` from Sharkord's WebSocket messages (simulcast turns the codec into VP8). A renamed key silently turns the helper off; `auto` never uses it. A signal from the server or the SDP would remove the dependency. |
| 18 | P2 | NVIDIA and X11 on Linux | 💡 | NVIDIA: needs NVIDIA's driver (nouveau can't encode); FFmpeg's `h264_nvenc` or NVIDIA's Vulkan video would cover it (#7). X11 sessions use the source grid. |
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
| 55 | P1 | Cache Cargo | 📋 | The helper's FFmpeg (and Mesa on Linux, `build/deps`) is cached by `scripts/deps/**` on both platforms (#103, #104). Still missing: `~/.cargo` and `native/target`. |
| 56 | P1 | Lint and format checks | 📋 | ESLint (repo style: no semicolons) for `electron/`, `cargo fmt --check` + `cargo clippy` for `native/`. There are no lint or test scripts today. |
| 38 | P1 | Tests for the pairing logic | 📋 | The worker's frame pairing is the most fragile code and needs no GPU: feed it synthetic frames (drops, timestamp drift, keyframes, PLIs) in CI. |
| 58 | P1 | Installer smoke test | 📋 | Install the NSIS build on the runner, launch it, assert a local page loads and the app quits cleanly. |
| 39 | P1 | Code signing | 📋 | Unsigned installer plus a spawned `.exe` trips SmartScreen and antivirus tools (README already warns about SmartScreen). |
| 42 | P2 | Auto-update | 💡 | electron-updater with GitHub Releases works for NSIS and AppImage (not Flatpak). |
| 59 | P2 | Checksums and provenance | 💡 | SHA-256 sums and GitHub artifact attestations with each Release. |
| 60 | P2 | ARM64 builds | 💡 | electron-builder handles NSIS and AppImage arm64; the native helper would need an ARM encoder. |
| 41 | P2 | Licence notices | 🚧 | The helper ships `LICENSES/` with FFmpeg's LGPL (and `SOURCES.md`: source, configure line and patches) on both platforms, plus glslang's and Mesa's on Linux (#104), libvpl's and the AMF/NVENC headers' on Windows (#103). Missing: a top-level third-party notice and an About entry. |
| 40 | P2 | Helper size | 💡 | Windows: check the staged plugin set (`PLUGINS` in `stage-native.js`) per encoder once #7/#8 add plugins (or drop it with #103). Linux: the helper is ~10 MB stripped, the bundled RADV/ANV ~21/26 MB (~4 MB each compressed). |
| 61 | P3 | Flatpak / AUR | 💡 | electron-builder's Flatpak target makes single-file bundles only, with no auto-update; Flathub would need its own manifest. |
| 105 | P2 | Linux builds on Ubuntu 24.04 | 📋 | The Linux job builds on Ubuntu 22.04 on purpose: its glibc (2.35) is the oldest the app then runs on (Ubuntu 22.04+, Debian 12+, Mint 21+). A newer image would drop those. Move to 24.04 (glibc 2.39) when 22.04's standard support ends in April 2027; `scripts/deps` already builds its own SPIRV-Tools, glslang and SPA headers, and Mesa with clang, because 22.04's are too old. |
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
| 68 | P2 | Contributor guide | ✅ | `CONTRIBUTING.md`: prerequisites, building, CI, releases and the project structure, moved out of the README, which is now for users only. |

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
